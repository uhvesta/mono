use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

/// How long sqlite will internally retry on `SQLITE_BUSY` before
/// surfacing the error to the caller. We funnel concurrent CLI writes
/// against the same `state.db` (multiple `boss chore bind-pr` etc.
/// landing in the engine in parallel) — without this the second writer
/// would fail with "database is locked" even though the first writer
/// finishes in microseconds. Five seconds is overkill for the in-engine
/// case (writes are tiny) but cheap when uncontended.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Sliding window for the merge-conflict churn-guard heuristic
/// (`merge-conflict-handling-in-review.md` Q6 / Phase 6 #16): the
/// 4th `conflict_resolutions` row for a given work item inside one
/// hour is created as `abandoned` instead of `pending`.
pub const CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window count at which the next attempt is pre-abandoned.
/// The first 3 attempts inside `CHURN_GUARD_WINDOW_SECS` go live; the
/// 4th trips the guard.
pub const CHURN_GUARD_THRESHOLD: i64 = 3;
/// `failure_reason` stamped on the pre-abandoned row.
pub const CHURN_GUARD_REASON: &str = "churn_threshold_exceeded";

/// Sliding window for the orphan-active redispatch churn guard: the
/// 4th orphan-redispatch for a given work item inside one hour is
/// skipped and a warning is logged instead.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window terminal-execution count at which the next
/// orphan-redispatch is skipped. The first 3 cycles inside the window
/// go live; the 4th trips the guard.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD: i64 = 3;

/// Sliding window for the duplicate-create guard: a non-deleted task/chore
/// in the same product with the same name created within this many seconds
/// of the attempted insert causes a `DuplicateTaskError` unless
/// `force_duplicate` is set on the input.
pub const DUPLICATE_GUARD_WINDOW_SECS: i64 = 60;

pub use boss_protocol::{
    AddDependencyInput, CREATED_VIA_ENGINE_AUTO, CREATED_VIA_UNKNOWN, CiRemediation,
    ConflictResolution, CreateAttentionItemInput, CreateChoreInput, CreateExecutionInput,
    CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput,
    CreateRunInput, CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter,
    EffortLevel, ExecutionReconcileResult, ListDependenciesInput, Product, Project,
    ProjectDesignDocState, RemoveDependencyInput, RequestExecutionInput,
    ResolveProjectDesignDocOutput, ResolvedDesignDoc, ResolvedDesignDocKind,
    SetProjectDesignDocInput, Task, TaskRuntime, WorkAttentionItem, WorkExecution, WorkItem,
    WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView, WorkItemExternalRef,
    WorkItemPatch, WorkRun, WorkTree, is_known_created_via,
};

/// Outcome of `WorkDb::record_pre_start_failure`. The coordinator uses
/// this to decide whether to schedule a delayed kick (retry) or surface
/// a permanent failure to the operator.
#[derive(Debug, Clone, PartialEq)]
pub enum PreStartFailureOutcome {
    /// The execution has been reset to `ready` with a `dispatch_not_before`
    /// delay. The coordinator should kick the scheduler after `delay`.
    Retry { delay: Duration },
    /// All retry attempts exhausted. The execution is now `failed`.
    /// The coordinator should surface an attention item.
    PermanentFail,
}

/// Returned by `insert_task_in_tx` / `insert_chore_in_tx` when the
/// duplicate guard fires. Carried as an `anyhow::Error` so `app.rs` can
/// downcast and send a structured `WorkItemDuplicateBlocked` event.
#[derive(Debug)]
pub struct DuplicateTaskError {
    pub existing_id: String,
    pub existing_short_id: i64,
    pub name: String,
    pub age_secs: i64,
}

impl std::fmt::Display for DuplicateTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A task/chore named {:?} was created {} seconds ago (id: {}, short_id: T{}); \
             pass --force-duplicate to create another",
            self.name, self.age_secs, self.existing_id, self.existing_short_id,
        )
    }
}

impl std::error::Error for DuplicateTaskError {}

use crate::work_dependencies::{self as deps, EdgeInsertOutcome, RELATION_BLOCKS};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MEM_DB_ID: AtomicU64 = AtomicU64::new(1);

/// Keeps a named shared-cache in-memory SQLite database alive. `Connection`
/// is `Send` but not `Sync`; wrapping in `Mutex` makes the anchor `Sync`.
/// `Arc` lets `WorkDb::clone` share the anchor across copies of the same
/// in-memory database (needed by the concurrent-insert test).
#[derive(Clone)]
struct InMemoryAnchor {
    uri: String,
    _conn: Arc<Mutex<Connection>>,
}

pub struct WorkDb {
    path: PathBuf,
    /// Present only when the database is in-memory (path == ":memory:").
    memory: Option<InMemoryAnchor>,
}

impl Clone for WorkDb {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            memory: self.memory.clone(),
        }
    }
}

impl WorkDb {
    pub fn open(path: PathBuf) -> Result<Self> {
        if path == Path::new(":memory:") {
            return Self::open_in_memory();
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work db directory {}", parent.display())
            })?;
        }

        let db = Self { path, memory: None };
        db.init()?;
        Ok(db)
    }

    /// Create a per-call named shared-cache in-memory database. Each call
    /// gets a unique name so parallel tests never share state. The anchor
    /// connection keeps the database alive until the `WorkDb` is dropped.
    fn open_in_memory() -> Result<Self> {
        let id = NEXT_MEM_DB_ID.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file:boss_mem_{id}?mode=memory&cache=shared");
        let anchor = Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("failed to open in-memory db {uri}"))?;
        let db = Self {
            path: PathBuf::from(":memory:"),
            memory: Some(InMemoryAnchor {
                uri,
                _conn: Arc::new(Mutex::new(anchor)),
            }),
        };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list_products(&self) -> Result<Vec<Product>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model, dispatch_preamble, external_tracker_kind, external_tracker_config
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
        let repo_remote_url = canonicalize_repo_remote_url(input.repo_remote_url);

        tx.execute(
            "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6, NULL)",
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
            "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                    design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
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
        let short_id = allocate_short_id(&tx, &input.product_id)?;

        tx.execute(
            "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, short_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', 'medium', ?7, ?7, ?8)",
            params![id, input.product_id, input.name, slug, description, goal, now, short_id],
        )?;

        // Auto-create the project's design task unless the caller
        // opted out with `no_design_task`. For design-shaped projects
        // the task sorts first (ordinal = 0) so the dispatcher picks
        // it up before the project's own tasks (ordinal ≥ 1).
        // Non-design-shaped projects (postmortems, checklists, etc.)
        // pass `no_design_task = true` and land here with zero tasks.
        if !input.no_design_task {
            insert_design_task_for_project_in_tx(
                &tx,
                &input.product_id,
                &id,
                &input.name,
                input.autostart,
            )?;
        }

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
        // Pre-check the resolver outside the transaction. If the work
        // item has no repo resolution, write a sticky attention item
        // via a *separate* short-lived tx that commits before the
        // bail unwinds — the kanban Attention lane must surface the
        // same failure the CLI exit code does, per multi-repo Q5.
        // Doing this inside the dispatch tx would lose the attention
        // item to the rollback.
        if resolve_repo_for_work_item(&conn, &input.work_item_id)?.is_none() {
            let label = repo_unresolved_kind_label(&conn, &input.work_item_id)?;
            let attn_tx = conn.transaction()?;
            record_repo_unresolved_attention(&attn_tx, &input.work_item_id, label)?;
            attn_tx.commit()?;
            bail!(
                "{}",
                repo_unresolved_attention_body(&input.work_item_id, label)
            );
        }
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
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
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
        let mut stmt = conn.prepare(
            "SELECT t.id FROM tasks t
             WHERE t.status = 'active'
               AND t.deleted_at IS NULL
               AND CAST(t.updated_at AS INTEGER) < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.status = 'ready'
               )
             ORDER BY t.updated_at ASC, t.id ASC",
        )?;
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
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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

    /// Fetch a single product row by id. Returns `None` when no row
    /// matches so the dispatcher can fall through the design's Q3
    /// precedence (product default → engine default) without
    /// distinguishing "no product default set" from "the row's
    /// product id doesn't resolve" — both produce the same engine
    /// fall-through behaviour for the spawn config resolver.
    pub fn get_product(&self, id: &str) -> Result<Option<Product>> {
        let conn = self.connect()?;
        query_product(&conn, id)
    }

    /// Set (or clear) a product's `default_model` per the
    /// effort-and-model-estimation design (PR #370). `model = None`
    /// or `Some("")` clears the column; any other slug is stored
    /// verbatim after a trim. The engine deliberately does NOT
    /// validate the slug — `claude` is the source of truth on what
    /// `--model` accepts, and a new model must be adoptable without
    /// an engine release blocking it (design §Q3).
    pub fn set_product_default_model(
        &self,
        product_id: &str,
        model: Option<&str>,
    ) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        let now = now_string();
        let stored = model
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        tx.execute(
            "UPDATE products SET default_model = ?2, updated_at = ?3 WHERE id = ?1",
            params![product_id, stored, now],
        )?;
        let updated = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// Bind (or unbind) a product's external tracker columns.
    ///
    /// When `unset = true`: clears both `external_tracker_kind` and
    /// `external_tracker_config` to NULL regardless of any other fields.
    /// When `unset = false`: both `kind` and `config` must be `Some`;
    /// the engine stores `config` as its JSON string representation.
    pub fn set_product_external_tracker(
        &self,
        product_id: &str,
        kind: Option<&str>,
        config: Option<&serde_json::Value>,
        unset: bool,
    ) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        let now = now_string();
        if unset {
            tx.execute(
                "UPDATE products SET external_tracker_kind = NULL, external_tracker_config = NULL, updated_at = ?2 WHERE id = ?1",
                params![product_id, now],
            )?;
        } else {
            let config_json = config.map(|c| c.to_string());
            tx.execute(
                "UPDATE products SET external_tracker_kind = ?2, external_tracker_config = ?3, updated_at = ?4 WHERE id = ?1",
                params![product_id, kind, config_json, now],
            )?;
        }
        let updated = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// Write the project's design-doc pointer columns.
    ///
    /// Three input shapes (matching `SetProjectDesignDocInput`):
    /// - `unset = true` → all three columns are cleared to `NULL`
    ///   atomically. Any explicit field values supplied alongside are
    ///   ignored.
    /// - `design_doc_path = Some(p)` with non-empty `p` → set the
    ///   pointer. `p` is validated per the design's Q8 rules (no
    ///   leading `/`, no `..` segments, must end in `.md` /
    ///   `.markdown`). `design_doc_repo_remote_url` and
    ///   `design_doc_branch` are best-effort overrides; `None` /
    ///   blank clears that column so resolution falls back to the
    ///   product. The repo URL is canonicalised the same way
    ///   `products.repo_remote_url` is (trim-normalise).
    /// - `design_doc_path = None` (and `unset = false`) → update only
    ///   the non-path columns. The existing path stays put. Useful
    ///   when the user is correcting a typo in just the repo or
    ///   branch fields.
    ///
    /// Last-writer-wins: a fresh call overwrites whatever was there.
    /// `updated_at` is stamped on every write. `last_status_actor` is
    /// intentionally untouched — pointer edits are property edits,
    /// not status transitions (Q10).
    pub fn set_project_design_doc(&self, input: SetProjectDesignDocInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let before = query_project(&tx, &input.project_id)?
            .with_context(|| format!("unknown project: {}", input.project_id))?;
        let now = now_string();

        if input.unset {
            tx.execute(
                "UPDATE projects
                 SET design_doc_repo_remote_url = NULL,
                     design_doc_branch = NULL,
                     design_doc_path = NULL,
                     updated_at = ?2
                 WHERE id = ?1",
                params![input.project_id, now],
            )?;
        } else {
            let repo = canonicalize_design_doc_repo_remote_url(input.design_doc_repo_remote_url);
            let branch = normalize_optional_text(input.design_doc_branch);

            match input.design_doc_path {
                Some(raw_path) => {
                    let path = validate_design_doc_path(&raw_path)?;
                    tx.execute(
                        "UPDATE projects
                         SET design_doc_repo_remote_url = ?2,
                             design_doc_branch = ?3,
                             design_doc_path = ?4,
                             updated_at = ?5
                         WHERE id = ?1",
                        params![input.project_id, repo, branch, path, now],
                    )?;
                }
                None => {
                    tx.execute(
                        "UPDATE projects
                         SET design_doc_repo_remote_url = ?2,
                             design_doc_branch = ?3,
                             updated_at = ?4
                         WHERE id = ?1",
                        params![input.project_id, repo, branch, now],
                    )?;
                }
            }
        }

        let updated = query_project(&tx, &input.project_id)?
            .with_context(|| format!("unknown project: {}", input.project_id))?;
        record_design_doc_audit(&tx, &input.project_id, &before, &updated, AUDIT_ACTOR_HUMAN, &now)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Resolve a project's design-doc pointer into the structured
    /// `ProjectDesignDocState` the UI consumes.
    ///
    /// Resolution rules (per design Q2):
    /// - `design_doc_path` is `NULL` → `NotSet` (UI hides the
    ///   affordance entirely).
    /// - Otherwise fall back to the product for any missing
    ///   `repo_remote_url` / `branch` override. Branch defaults to
    ///   `"main"` when neither the project nor (a future)
    ///   `products.docs_branch` supplies one.
    /// - If no repo can be resolved (project override `NULL` and
    ///   product's `repo_remote_url` `NULL`) → `Broken` with a
    ///   human-readable reason.
    /// - Classify the resolved repo against the project's product:
    ///   `SameProduct` when it matches, `OtherProduct` when another
    ///   Boss-tracked product owns the repo, `External` otherwise.
    ///
    /// `lookup_repo_workspace_path` is consulted only on the resolved
    /// path — pass a closure that asks cube for the absolute path of
    /// a workspace currently leased for the resolved `repo_remote_url`
    /// (or `None` when no workspace is leased). The macOS open
    /// dispatcher uses the returned path to fast-path into `$EDITOR` /
    /// the in-app renderer; when `None`, the affordance falls back to
    /// the GitHub web URL. In test/CLI contexts where cube isn't
    /// reachable, `|_| None` is the safe default.
    pub fn resolve_project_design_doc<F>(
        &self,
        project_id: &str,
        lookup_repo_workspace_path: F,
    ) -> Result<ResolveProjectDesignDocOutput>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let conn = self.connect()?;
        let project = query_project(&conn, project_id)?
            .with_context(|| format!("unknown project: {project_id}"))?;
        let product = query_product(&conn, &project.product_id)?
            .with_context(|| format!("unknown product: {}", project.product_id))?;

        let Some(path) = project.design_doc_path.clone() else {
            return Ok(ResolveProjectDesignDocOutput {
                project_id: project.id,
                state: ProjectDesignDocState::NotSet,
            });
        };

        let resolved_repo = project
            .design_doc_repo_remote_url
            .clone()
            .or_else(|| product.repo_remote_url.clone());
        let Some(repo) = resolved_repo else {
            return Ok(ResolveProjectDesignDocOutput {
                project_id: project.id,
                state: ProjectDesignDocState::Broken {
                    reason: "design_doc_path is set but neither the project's design_doc_repo_remote_url nor the product's repo_remote_url is populated".to_owned(),
                },
            });
        };

        let branch = project
            .design_doc_branch
            .clone()
            .unwrap_or_else(|| "main".to_owned());

        let kind = if let Some(product_repo) = product.repo_remote_url.as_deref()
            && product_repo == repo.as_str()
        {
            ResolvedDesignDocKind::SameProduct {
                product_id: project.product_id.clone(),
            }
        } else if let Some(other_product) = find_product_by_repo_remote_url(&conn, &repo)? {
            ResolvedDesignDocKind::OtherProduct {
                product_id: other_product,
            }
        } else {
            ResolvedDesignDocKind::External
        };

        let web_url = render_design_doc_web_url(&repo, &branch, &path);
        let raw_content_url = render_design_doc_raw_content_url(&repo, &branch, &path);
        let workspace_path = lookup_repo_workspace_path(&repo);

        Ok(ResolveProjectDesignDocOutput {
            project_id: project.id,
            state: ProjectDesignDocState::Resolved {
                resolved: ResolvedDesignDoc {
                    repo_remote_url: repo,
                    branch,
                    path,
                    kind,
                },
                workspace_path,
                web_url,
                raw_content_url,
            },
        })
    }

    /// Sync a `(repo, branch, path)` triple discovered by
    /// `DesignDetector` into the parent project's pointer columns,
    /// **iff** the project's `design_doc_path` is currently `NULL`.
    ///
    /// This is the one-way auto-populate rule from design Q6: a
    /// project that already has a hand-set pointer wins; a project
    /// that has no pointer benefits from the detector's discovery.
    /// Repo URL is canonicalised on the way in; path is validated
    /// against the same Q8 rules `set_project_design_doc` enforces,
    /// so a detector that hands us a garbage path fails fast rather
    /// than corrupting the column.
    ///
    /// Returns `true` if the columns were written, `false` if the
    /// project already had a pointer set (no-op).
    ///
    /// TODO(design-producing-tasks): wire this method into
    /// `DesignDetector`'s `DOC_REF` stop handler once that detector
    /// exists. Until then, this is exercised only by integration
    /// tests with a hand-rolled caller.
    pub fn sync_project_design_doc_from_detector(
        &self,
        project_id: &str,
        repo_remote_url: Option<&str>,
        branch: Option<&str>,
        path: &str,
    ) -> Result<bool> {
        let validated_path = validate_design_doc_path(path)?;
        let repo = canonicalize_design_doc_repo_remote_url(repo_remote_url.map(str::to_owned));
        let branch = normalize_optional_text(branch.map(str::to_owned));

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let before = query_project(&tx, project_id)?
            .with_context(|| format!("unknown project: {project_id}"))?;
        if before.design_doc_path.is_some() {
            return Ok(false);
        }
        let now = now_string();
        tx.execute(
            "UPDATE projects
             SET design_doc_repo_remote_url = ?2,
                 design_doc_branch = ?3,
                 design_doc_path = ?4,
                 updated_at = ?5
             WHERE id = ?1",
            params![project_id, repo, branch, validated_path, now],
        )?;
        let after = query_project(&tx, project_id)?
            .with_context(|| format!("unknown project: {project_id}"))?;
        record_design_doc_audit(
            &tx,
            project_id,
            &before,
            &after,
            AUDIT_ACTOR_DESIGN_DETECTOR,
            &now,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Read the append-only audit trail of property edits on
    /// `project_id`. Returns rows in chronological order (oldest
    /// first), with a stable secondary sort on row id so two writes
    /// landing in the same `changed_at` second still serialise.
    ///
    /// v1 records design-doc pointer columns
    /// (`design_doc_repo_remote_url`, `design_doc_branch`,
    /// `design_doc_path`); the schema is general so future edits to
    /// other project properties can ride along without a re-migration.
    pub fn list_project_property_audit(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectPropertyAuditEntry>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, property, old_value, new_value, actor, changed_at
             FROM project_property_audit
             WHERE project_id = ?1
             ORDER BY changed_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(ProjectPropertyAuditEntry {
                id: row.get(0)?,
                project_id: row.get(1)?,
                property: row.get(2)?,
                old_value: row.get(3)?,
                new_value: row.get(4)?,
                actor: row.get(5)?,
                changed_at: row.get(6)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Surface a `WorkAttentionItem` when an `ApproveDesign` event
    /// names a design doc whose location disagrees with the parent
    /// project's already-set pointer.
    ///
    /// Behaviour (per design Q6 sync rule 3):
    /// - Project pointer is `NULL` → no conflict, no item, returns
    ///   `Ok(None)`. The auto-populate path
    ///   (`sync_project_design_doc_from_detector`) handles that
    ///   case before approval.
    /// - Project pointer matches the approved triple (after
    ///   resolving `None` overrides against the product / default
    ///   branch) → no item, returns `Ok(None)`.
    /// - Project pointer differs → an attention item with kind
    ///   `design_doc_pointer_conflict` is inserted against
    ///   `execution_id` and returned.
    ///
    /// The helper does NOT overwrite the project's pointer — the
    /// user's manual value always wins; the attention item asks
    /// them to choose explicitly.
    ///
    /// TODO(design-producing-tasks): wire this method into
    /// `ApproveDesign`'s state-transition handler once that path
    /// exists. Until then, this is exercised only by integration
    /// tests with a hand-rolled caller.
    pub fn surface_design_doc_conflict_on_approve(
        &self,
        project_id: &str,
        execution_id: &str,
        approved_repo_remote_url: Option<&str>,
        approved_branch: Option<&str>,
        approved_path: &str,
    ) -> Result<Option<WorkAttentionItem>> {
        let approved_path = validate_design_doc_path(approved_path)?;
        let approved_repo =
            canonicalize_design_doc_repo_remote_url(approved_repo_remote_url.map(str::to_owned));
        let approved_branch = normalize_optional_text(approved_branch.map(str::to_owned));

        let conn = self.connect()?;
        let project = query_project(&conn, project_id)?
            .with_context(|| format!("unknown project: {project_id}"))?;
        let Some(project_path) = project.design_doc_path.clone() else {
            return Ok(None);
        };
        let product = query_product(&conn, &project.product_id)?
            .with_context(|| format!("unknown product: {}", project.product_id))?;
        drop(conn);

        let project_repo_effective = project
            .design_doc_repo_remote_url
            .clone()
            .or_else(|| product.repo_remote_url.clone());
        let approved_repo_effective = approved_repo
            .clone()
            .or_else(|| product.repo_remote_url.clone());

        let project_branch_effective = project
            .design_doc_branch
            .clone()
            .unwrap_or_else(|| "main".to_owned());
        let approved_branch_effective = approved_branch
            .clone()
            .unwrap_or_else(|| "main".to_owned());

        if project_repo_effective == approved_repo_effective
            && project_branch_effective == approved_branch_effective
            && project_path == approved_path
        {
            return Ok(None);
        }

        let title = "Design doc pointer disagrees with approved design".to_owned();
        let body_markdown = format!(
            "The project's design-doc pointer (`{project_repo}` `{project_branch}` `{project_path}`) differs from the location of the approved design doc (`{approved_repo}` `{approved_branch}` `{approved_path}`). Update the project pointer with `boss project set-design-doc` or revoke the approval.",
            project_repo = project_repo_effective
                .as_deref()
                .unwrap_or("<no repo resolved>"),
            project_branch = project_branch_effective,
            project_path = project_path,
            approved_repo = approved_repo_effective
                .as_deref()
                .unwrap_or("<no repo resolved>"),
            approved_branch = approved_branch_effective,
            approved_path = approved_path,
        );

        let item = self.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution_id.to_owned()),
            work_item_id: None,
            kind: "design_doc_pointer_conflict".to_owned(),
            status: None,
            title,
            body_markdown,
            resolved_at: None,
        })?;
        Ok(Some(item))
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
        let existing = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
             FROM work_executions
             WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
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
        let _product = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
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
            match task.kind.as_str() {
                "chore" => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            "chore_implementation",
                            "ready",
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
               AND status NOT IN ('done', 'archived', 'blocked')",
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

    /// Record the head SHA of the chore's bound PR captured at run
    /// start. Used by the Stop-boundary SHA-delta gate to decide
    /// whether a resume run actually contributed to the bound PR
    /// before falling through to the `PROBE_NO_PR` nudge. Idempotent;
    /// callers may invoke once per execution start (or skip when no
    /// PR is bound). Empty `sha` is rejected — pass `None` semantics
    /// by simply not calling.
    pub fn set_execution_pr_head_before(
        &self,
        execution_id: &str,
        sha: &str,
    ) -> Result<()> {
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
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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
    pub fn transcript_path_for_execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<String>> {
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
        query_run(&conn, run_id)?.with_context(|| format!("unknown run: {run_id}"))
    }

    pub fn create_attention_item(
        &self,
        input: CreateAttentionItemInput,
    ) -> Result<WorkAttentionItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let (execution_id, work_item_id) = attention_target_from_input(&tx, &input)?;

        let id = next_id("attn");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "open".to_owned());
        let resolved_at = normalize_optional_text(input.resolved_at);

        tx.execute(
            "INSERT INTO work_attention_items (
                id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                execution_id,
                work_item_id,
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
            "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_attention_item)?;
        collect_rows(rows)
    }

    /// List the sticky, pre-dispatch attention items attached to a
    /// work item (i.e. `work_item_id IS NOT NULL`). Used by the
    /// `repo_unresolved` surface and any future work-item-scoped
    /// attention flows. Errors if the work item id is unknown so
    /// callers can't accidentally silently no-op on a typo.
    pub fn list_attention_items_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Vec<WorkAttentionItem>> {
        let conn = self.connect()?;
        let _ = product_id_for_work_item(&conn, work_item_id)?;
        let mut stmt = conn.prepare(
            "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE work_item_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([work_item_id], map_attention_item)?;
        collect_rows(rows)
    }

    pub fn get_attention_item(&self, id: &str) -> Result<WorkAttentionItem> {
        let conn = self.connect()?;
        query_attention_item(&conn, id)?.with_context(|| format!("unknown attention item: {id}"))
    }

    pub fn update_work_item(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        self.update_work_item_as_actor(id, patch, "human")
    }

    /// Like `update_work_item` but stamps `last_status_actor` with `actor`
    /// when the status actually changes. Engine-internal writers use direct
    /// SQL with `last_status_actor = 'engine'`; this path is for peer RPCs
    /// where the caller tier has already been resolved to `"human"` or
    /// `"boss"`.
    pub fn update_work_item_as_actor(
        &self,
        id: &str,
        patch: WorkItemPatch,
        actor: &str,
    ) -> Result<WorkItem> {
        match classify_id(id)? {
            ItemKind::Product => self.update_product(id, patch),
            ItemKind::Project => self.update_project(id, patch, actor),
            ItemKind::Task => self.update_task(id, patch, actor),
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
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                        design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
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
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
                 FROM tasks
                 WHERE product_id = ?1 AND kind IN ('project_task', 'design') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        let chores = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
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

    /// Single-item version of the per-task runtime data carried in
    /// `WorkTree::task_runtimes`. Backs the `GetTaskRuntime` RPC that
    /// `boss chore show` / `boss task show` use to surface the active
    /// execution + run on the rendered work item. The lookup never
    /// fails on missing executions: an untouched work item simply
    /// returns a `TaskRuntime` with every `Option` field set to
    /// `None`. Friendly ids (`T42`, `boss/42`) are resolved to primary
    /// ids before the query runs, matching `get_work_item`'s contract.
    pub fn get_task_runtime(&self, work_item_id: &str) -> Result<TaskRuntime> {
        let conn = self.connect()?;
        let resolved = resolve_friendly_work_item_id(&conn, work_item_id)?
            .unwrap_or_else(|| work_item_id.to_owned());
        query_task_runtime(&conn, &resolved)
    }

    /// Look up a work item by its per-product short_id. Searches both
    /// the `tasks` table (returning `Task` or `Chore`) and the
    /// `projects` table, returning the first match. Returns `None` if
    /// no row with `(product_id, short_id)` exists.
    ///
    /// The per-product sequence is shared across tasks and projects
    /// (design Q1), so each short_id belongs to at most one row across
    /// both tables for a given product.
    pub fn get_work_item_by_short_id(
        &self,
        product_id: &str,
        short_id: i64,
    ) -> Result<Option<WorkItem>> {
        let conn = self.connect()?;
        if let Some(task) = conn
            .query_row(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
                 FROM tasks
                 WHERE product_id = ?1 AND short_id = ?2 AND deleted_at IS NULL",
                params![product_id, short_id],
                map_task,
            )
            .optional()?
        {
            return Ok(Some(task_to_item(task)));
        }
        if let Some(project) = conn
            .query_row(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                        design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
                 FROM projects
                 WHERE product_id = ?1 AND short_id = ?2",
                params![product_id, short_id],
                map_project,
            )
            .optional()?
        {
            return Ok(Some(WorkItem::Project(project)));
        }
        Ok(None)
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
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
                 FROM tasks
                 WHERE product_id = ?1 AND project_id = ?2 AND kind IN ('project_task', 'design') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(params![product_id, project_id], map_task)?;
            collect_rows(rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
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
    /// Read a value from the engine's metadata KV. Returns `None` if
    /// the key has never been written. Used by the engine for small
    /// persisted settings (live-status disabled slot list, schema
    /// version, etc.) that don't deserve their own table.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert-or-replace a metadata value. The metadata table is the
    /// engine-side KV store — schema version, persisted live-status
    /// disabled slots, anything that needs to outlive the process
    /// without justifying a dedicated table.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

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
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
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
        // gone and the engine itself put this row in `blocked` (identified
        // by blocked_reason='dependency'), flip it back to `todo`.
        // Human-placed blocks (other blocked_reason / NULL + human actor)
        // stick — the user must clear them.
        let now = now_string();
        maybe_engine_unblock_dependent(&tx, dependent_id, &now)?;
        tx.commit()?;
        Ok(removed)
    }

    /// All task ids that are currently in `blocked` status because of
    /// a dependency edge the engine set — i.e. rows that the periodic
    /// dependency-unblock sweeper should evaluate. Returns
    /// `(task_id, updated_at_epoch_secs)` so the sweeper can compute
    /// how long each row has been stuck.
    ///
    /// The candidate set is:
    ///   - `blocked_reason = 'dependency'`  — set by `maybe_engine_block_dependent`
    ///   - `blocked_reason IS NULL AND last_status_actor = 'engine'`  — pre-backfill rows
    pub fn list_dependency_blocked_candidates(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, CAST(updated_at AS INTEGER)
             FROM tasks
             WHERE status = 'blocked'
               AND deleted_at IS NULL
               AND (
                   blocked_reason = 'dependency'
                   OR (blocked_reason IS NULL AND last_status_actor = 'engine')
               )
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Check whether `work_item_id` is still gated by unsatisfied
    /// prerequisites. If not — all prereqs are done and the block was
    /// engine-owned — flip the item to `todo` and return `true`.
    /// Returns `false` without modifying the DB when the item is not
    /// blocked, is human-blocked, or still has gating prereqs.
    ///
    /// Used by the periodic dependency-unblock sweeper as a per-item
    /// fallback for the case where the event-driven cascade
    /// ([`cascade_dependents_after_prereq_status_change`]) silently
    /// skipped this row (e.g. `last_status_actor` mismatch from a
    /// concurrent update, or engine was offline when the prereq landed).
    pub fn try_unblock_dependency_if_resolved(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let unblocked = maybe_engine_unblock_dependent(&tx, work_item_id, &now)?;
        tx.commit()?;
        Ok(unblocked)
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
                updated_at TEXT NOT NULL,
                default_model TEXT,
                ci_attempt_budget INTEGER NOT NULL DEFAULT 3,
                dispatch_preamble TEXT,
                external_tracker_kind TEXT,
                external_tracker_config TEXT
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
                updated_at TEXT NOT NULL,
                design_doc_repo_remote_url TEXT,
                design_doc_branch TEXT,
                design_doc_path TEXT
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
                priority TEXT NOT NULL DEFAULT 'medium',
                repo_remote_url TEXT,
                created_via TEXT NOT NULL DEFAULT 'unknown',
                effort_level TEXT,
                model_override TEXT,
                ci_attempt_budget INTEGER,
                ci_attempts_used INTEGER NOT NULL DEFAULT 0,
                external_ref_kind TEXT,
                external_ref_canonical_id TEXT,
                external_ref_raw TEXT,
                external_ref_synced_at TEXT,
                external_ref_unbound_at TEXT
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
                execution_id TEXT REFERENCES work_executions(id) ON DELETE CASCADE,
                work_item_id TEXT,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT NOT NULL,
                body_markdown TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT,
                CHECK (
                    (execution_id IS NOT NULL AND work_item_id IS NULL)
                    OR (execution_id IS NULL AND work_item_id IS NOT NULL)
                )
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

            CREATE TABLE IF NOT EXISTS project_property_audit (
                id          TEXT PRIMARY KEY,
                project_id  TEXT NOT NULL,
                property    TEXT NOT NULL,
                old_value   TEXT,
                new_value   TEXT,
                actor       TEXT NOT NULL,
                changed_at  TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS project_property_audit_project_idx
                ON project_property_audit(project_id, changed_at);
            ",
        )?;
        migrate_work_executions_v3(&conn)?;
        migrate_tasks_autostart(&conn)?;
        migrate_last_status_actor(&conn)?;
        migrate_tasks_priority(&conn)?;
        migrate_project_design_doc_columns(&conn)?;
        migrate_tasks_created_via(&conn)?;
        migrate_backfill_project_design_tasks(&conn)?;
        migrate_tasks_repo_remote_url(&conn)?;
        migrate_project_property_audit_table(&conn)?;
        // Index creation must follow migration: pre-v3 databases don't
        // have `priority` until `migrate_work_executions_v3` adds it,
        // and SQLite's `CREATE INDEX IF NOT EXISTS` errors on missing
        // columns rather than silently skipping. Keep this out of the
        // schema-init batch so a pre-v3 database can still be opened.
        // The same rule applies to `tasks_repo_idx` against pre-v5
        // databases that haven't yet been migrated.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS work_executions_ready_idx
                ON work_executions(status, priority, created_at)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS tasks_repo_idx
                ON tasks(repo_remote_url, deleted_at)
                WHERE repo_remote_url IS NOT NULL",
            [],
        )?;
        migrate_timestamps_to_epoch(&conn)?;
        migrate_tasks_blocked_reason(&conn)?;
        migrate_products_auto_pr_maintenance_enabled(&conn)?;
        migrate_conflict_resolutions_table(&conn)?;
        migrate_backfill_blocked_reason_dependency(&conn)?;
        migrate_work_attention_items_work_item_id(&conn)?;
        migrate_tasks_effort_and_model_columns(&conn)?;
        migrate_products_default_model(&conn)?;
        migrate_task_blocked_signals_table(&conn)?;
        migrate_ci_remediations_table(&conn)?;
        migrate_ci_failure_suppressions_table(&conn)?;
        migrate_tasks_ci_attempt_columns(&conn)?;
        migrate_products_ci_attempt_budget(&conn)?;
        migrate_products_dispatch_preamble(&conn)?;
        migrate_backfill_task_blocked_signals(&conn)?;
        migrate_effort_escalations_table(&conn)?;
        migrate_null_redundant_task_repo_remote_urls(&conn)?;
        // Runs last so the per-product `(created_at, id)` backfill
        // sees every task/project row that earlier migrations may
        // have inserted (notably `migrate_backfill_project_design_tasks`).
        migrate_short_id_columns(&conn)?;
        // Clears `autostart` on rows that have already been dispatched
        // so the single-shot semantics (AI #2, Incident 001) apply to
        // existing data too. Must run after `migrate_tasks_autostart`
        // so the column exists.
        migrate_backfill_autostart_consumed(&conn)?;
        // Engine counter-metrics framework (phase 1). Independent of
        // every other table — runs last because order doesn't matter
        // for `CREATE TABLE IF NOT EXISTS`.
        migrate_metrics_tables(&conn)?;
        migrate_work_executions_pre_start_retry(&conn)?;
        migrate_work_executions_pr_url(&conn)?;
        migrate_work_executions_pr_head_before(&conn)?;
        // PR poll state columns for CI + review indicators on Review-lane cards.
        migrate_pr_poll_state_columns(&conn)?;
        // External tracker binding columns (products) and per-work-item
        // upstream-ref columns (tasks) plus partial indices. Design:
        // tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md
        migrate_external_tracker_columns(&conn)?;
        // Host registry tables + work_executions host columns for distributed
        // agent execution (phase 1 — schema + CLI only, no dispatch change).
        // Design: tools/boss/docs/designs/distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md
        crate::host_registry::migrate_host_registry_tables(&conn)?;
        crate::host_registry::migrate_work_executions_host_columns(&conn)?;
        crate::host_registry::ensure_local_host(&conn)?;
        crate::host_registry::refresh_local_host_auto_capabilities(&conn)?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '12')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    pub(crate) fn connect(&self) -> Result<Connection> {
        let mut conn = if let Some(mem) = &self.memory {
            // For in-memory databases, connect via the named shared-cache URI
            // so every connect() call shares the same database instance.
            Connection::open_with_flags(
                &mem.uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .with_context(|| format!("failed to connect to in-memory db {}", mem.uri))?
        } else {
            Connection::open(&self.path)
                .with_context(|| format!("failed to open work db {}", self.path.display()))?
        };
        // WAL lets readers and writers coexist (read-side concurrency
        // is unaffected by an in-flight write) and `busy_timeout`
        // turns lock contention into latency rather than an error
        // returned to the caller. `synchronous = NORMAL` is the
        // recommended pairing for WAL — durable across application
        // crashes, only loses commits on OS/power loss, which is fine
        // for engine state we can rebuild.
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;\n\
             PRAGMA foreign_keys = ON;",
        )?;
        // Default writes to `BEGIN IMMEDIATE`. With the previous
        // `BEGIN DEFERRED`, two concurrent writers could each open a
        // read-mode transaction, then both try to upgrade to write,
        // and the loser fails with `SQLITE_BUSY_SNAPSHOT` — which the
        // busy-timeout handler does NOT retry. `IMMEDIATE` acquires
        // the write lock up front so the second caller waits inside
        // the busy handler instead of racing.
        conn.set_transaction_behavior(TransactionBehavior::Immediate);
        Ok(conn)
    }

    fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product =
            query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_repo_remote_url_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_text_patch(&mut product.status, patch.status);
        apply_optional_string_patch(&mut product.default_model, patch.default_model);
        apply_optional_string_patch(&mut product.dispatch_preamble, patch.dispatch_preamble);
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7, default_model = ?8, dispatch_preamble = ?9
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
                product.default_model,
                product.dispatch_preamble,
            ],
        )?;

        let updated = query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    fn update_project(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
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
        let actor_stamp = if status_changed && previous_status != project.status { actor } else { "" };

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
                actor_stamp,
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

    fn update_task(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
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
        // Reject non-empty repo override when the product has its own repo.
        if let Some(ref repo_patch) = patch.repo_remote_url {
            if !repo_patch.trim().is_empty() {
                let product = query_product(&tx, &task.product_id)?
                    .with_context(|| format!("orphan task {id}: parent product {} missing", task.product_id))?;
                if let Some(product_repo) = product.repo_remote_url.as_deref() {
                    bail!(
                        "cannot set per-task repo override on product `{}`: \
                         product has its own repo (`{}`). \
                         Clear the product's repo first, or omit --repo to inherit.",
                        product.slug,
                        product_repo,
                    );
                }
            }
        }
        apply_repo_remote_url_patch(&mut task.repo_remote_url, patch.repo_remote_url);
        if let Some(priority_patch) = patch.priority {
            task.priority = normalize_priority(Some(&priority_patch))?;
        }
        if let Some(effort_patch) = patch.effort_level {
            // Empty string clears the column; anything else must
            // parse as one of the five allowed levels. Invalid
            // values reject the whole patch — no half-updates.
            let trimmed = effort_patch.trim();
            task.effort_level = if trimmed.is_empty() {
                None
            } else {
                Some(
                    trimmed
                        .parse::<EffortLevel>()
                        .map_err(|e| anyhow::anyhow!(e))?,
                )
            };
        }
        apply_optional_string_patch(&mut task.model_override, patch.model_override);
        apply_optional_string_patch(&mut task.blocked_reason, patch.blocked_reason);
        if let Some(autostart) = patch.autostart {
            task.autostart = autostart;
        }
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, &previous_status, &task.status)?;
        }
        let actor_stamp = if status_changed && previous_status != task.status { actor } else { "" };

        let effort_level_value = task.effort_level.map(|level| level.as_str().to_owned());

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7,
                 priority = ?9, repo_remote_url = ?10,
                 effort_level = ?11, model_override = ?12, autostart = ?13,
                 blocked_reason = ?14,
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
                actor_stamp,
                task.priority,
                task.repo_remote_url,
                effort_level_value,
                task.model_override,
                task.autostart as i64,
                task.blocked_reason,
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
               AND t.kind IN ('chore', 'project_task', 'design')
               AND t.status = 'active'
               AND (t.pr_url IS NULL OR t.pr_url = '')
             ORDER BY we.created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
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
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after update: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Update the PR poll-state columns for a single task row after a
    /// successful merge-poller probe. Stores the CI and review state strings
    /// (and optional JSON-encoded detail blobs) plus the current timestamp.
    ///
    /// Returns `Ok(true)` when the CI or review state actually changed (so
    /// the caller should emit a change event), `Ok(false)` when the probe
    /// confirmed the same state as before (no event needed), and `Ok(false)`
    /// when the row was deleted or not found. Errors propagate from
    /// the underlying DB operations.
    ///
    /// The UPDATE is guarded by a `WHERE` clause that skips rows whose
    /// `ci_required_state` AND `review_required_state` are already set to
    /// the incoming values, so `changes() == 0` reliably means "nothing
    /// changed" — the caller does not need to issue a separate read.
    pub fn update_task_pr_poll_state(
        &self,
        work_item_id: &str,
        ci_required_state: &str,
        review_required_state: &str,
        ci_required_detail: Option<&str>,
        review_required_detail: Option<&str>,
        merge_queue_state: Option<&str>,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
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
        Ok(changed > 0)
    }

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
             WHERE kind IN ('chore', 'project_task', 'design')
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
             WHERE kind IN ('chore', 'project_task', 'design')
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
        let updated = query_task(&tx, work_item_id)?.with_context(|| {
            format!("unknown task after merge_conflict clear: {work_item_id}")
        })?;
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
        let updated = query_task(&tx, work_item_id)?.with_context(|| {
            format!("unknown task after merge_conflict clear: {work_item_id}")
        })?;
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

    fn mark_chore_blocked_ci_signal(
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
        let updated = query_task(&tx, work_item_id)?.with_context(|| {
            format!("unknown task after ci_failure clear: {work_item_id}")
        })?;
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
                 consumes_budget, failed_checks, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)",
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
                    created_at, started_at, finished_at
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

    /// Read the unified auto-maintenance opt-out flag for a product.
    /// Defaults to `true` when the column is unset or the product row
    /// is missing — i.e. the opt-out only takes effect when the
    /// operator has explicitly disabled it.
    ///
    /// Used by the conflict-watch (and, in later phases, ci-watch /
    /// auto-rebase) paths to skip auto-remediation for products whose
    /// owner has set `auto_pr_maintenance_enabled = 0`
    /// (`merge-conflict-handling-in-review.md` Q7 / Phase 6 #18).
    pub fn product_auto_pr_maintenance_enabled(&self, product_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let enabled: Option<i64> = conn
            .query_row(
                "SELECT auto_pr_maintenance_enabled FROM products WHERE id = ?1",
                params![product_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(enabled.map(|v| v != 0).unwrap_or(true))
    }

    /// True iff there's a non-terminal `rebase_attempts` row covering
    /// the given PR url. Used by `conflict_watch::on_conflict_detected`
    /// to defer when the `auto-rebase-stacked-prs` flow already owns
    /// the slot (design Q7).
    ///
    /// The `rebase_attempts` table ships with that flow, not this one.
    /// Until it lands, this method short-circuits to `false` so the
    /// dispatch site reads identically before and after auto-rebase
    /// is wired up.
    pub fn has_active_rebase_attempt_for_pr(&self, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        if !table_exists(&conn, "rebase_attempts")? {
            return Ok(false);
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rebase_attempts
              WHERE dependent_pr_url = ?1
                AND status IN ('pending', 'running', 'escalated')",
            params![pr_url],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Insert a `conflict_resolutions` row with `status='pending'`
    /// alongside a `tasks.blocked_attempt_id` pointer to the new
    /// attempt id. `(work_item_id, base_sha_at_trigger)` is the
    /// idempotency key — a second probe for the same `(item, sha)`
    /// finds the row already pending and returns `Ok(None)` (caller
    /// reads the existing row via [`Self::active_conflict_resolution_for_work_item`]).
    ///
    /// Phase 3 of the merge-conflict design (Q4). The caller is
    /// `conflict_watch::on_conflict_detected` after the parent
    /// `tasks` row is already flipped to `blocked: merge_conflict`.
    ///
    /// Churn guard (Phase 6 #16, design Q6): if the work item has
    /// already produced ≥ [`CHURN_GUARD_THRESHOLD`] conflict_resolutions
    /// rows in the trailing [`CHURN_GUARD_WINDOW_SECS`], the new row is
    /// inserted in `status='abandoned'` with
    /// `failure_reason='churn_threshold_exceeded'` so the dispatcher
    /// skips it and the parent stays `blocked` for human attention.
    pub fn insert_conflict_resolution(
        &self,
        input: ConflictResolutionInsertInput,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("crz");
        let now = now_string();

        // Count the trailing-1h attempts for this work item; if we've
        // already crossed the churn threshold, the new row is
        // pre-abandoned. The count is computed in the same transaction
        // as the insert so two concurrent probes can't both squeak past
        // the bar.
        let now_secs: i64 = now.parse().unwrap_or(0);
        let cutoff_secs = now_secs - CHURN_GUARD_WINDOW_SECS;
        let recent_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM conflict_resolutions
              WHERE work_item_id = ?1
                AND CAST(created_at AS INTEGER) >= ?2",
            params![input.work_item_id, cutoff_secs],
            |row| row.get(0),
        )?;
        let churn_tripped = recent_count >= CHURN_GUARD_THRESHOLD;
        let (status, failure_reason, finished_at): (&str, Option<&str>, Option<&str>) =
            if churn_tripped {
                ("abandoned", Some("churn_threshold_exceeded"), Some(now.as_str()))
            } else {
                ("pending", None, None)
            };

        let rows = tx.execute(
            "INSERT OR IGNORE INTO conflict_resolutions
                (id, product_id, work_item_id, pr_url, pr_number,
                 head_branch, base_branch, base_sha_at_trigger,
                 head_sha_before, status, failure_reason, created_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.head_branch,
                input.base_branch,
                input.base_sha_at_trigger,
                input.head_sha_before,
                status,
                failure_reason,
                now,
                finished_at,
            ],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        // Only stamp the parent's `blocked_attempt_id` for live
        // attempts; an immediately-abandoned churn-guard row would
        // mis-point the kanban at a dead attempt.
        if !churn_tripped {
            tx.execute(
                "UPDATE tasks
                    SET blocked_attempt_id = ?2,
                        updated_at         = ?3
                  WHERE id = ?1
                    AND status = 'blocked'
                    AND blocked_reason = 'merge_conflict'
                    AND deleted_at IS NULL",
                params![input.work_item_id, id, now],
            )?;
        }
        let inserted = query_conflict_resolution(&tx, &id)?
            .with_context(|| format!("unknown conflict_resolution after insert: {id}"))?;
        tx.commit()?;
        Ok(Some(inserted))
    }

    /// Fetch a single attempt row by id. `Ok(None)` if the row is
    /// missing.
    pub fn get_conflict_resolution(&self, attempt_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        query_conflict_resolution(&conn, attempt_id)
    }

    /// Latest non-terminal attempt for `work_item_id`. Used by the
    /// conflict-detection path to detect "an attempt is already in
    /// flight" and by the worker prompt composer to find the row to
    /// embed the diagnosis from.
    pub fn active_conflict_resolution_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                    base_sha_at_trigger, head_sha_before, head_sha_after, status, failure_reason,
                    cube_lease_id, cube_workspace_id, worker_id, conflict_diagnosis,
                    created_at, started_at, finished_at
             FROM conflict_resolutions
             WHERE work_item_id = ?1
               AND status IN ('pending', 'running')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([work_item_id], map_conflict_resolution)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Return every `conflict_resolutions` row that is `status='pending'`
    /// but has no `work_executions` row with `kind='conflict_resolution'`
    /// for the same `work_item_id`. Used by the startup backfill that
    /// recovers attempts orphaned before `on_conflict_detected` began
    /// writing the execution request in the same call (pre-PR #430).
    ///
    /// The query is idempotent: once an execution is created the row no
    /// longer satisfies the `NOT EXISTS` predicate and is excluded on
    /// every subsequent call.
    pub fn pending_conflict_resolutions_without_execution(
        &self,
    ) -> Result<Vec<ConflictResolution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT cr.id, cr.product_id, cr.work_item_id, cr.pr_url, cr.pr_number,
                    cr.head_branch, cr.base_branch, cr.base_sha_at_trigger,
                    cr.head_sha_before, cr.head_sha_after, cr.status, cr.failure_reason,
                    cr.cube_lease_id, cr.cube_workspace_id, cr.worker_id,
                    cr.conflict_diagnosis, cr.created_at, cr.started_at, cr.finished_at
             FROM conflict_resolutions cr
             WHERE cr.status = 'pending'
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = cr.work_item_id
                     AND we.kind = 'conflict_resolution'
               )",
        )?;
        let rows = stmt.query_map([], map_conflict_resolution)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Store the engine-collected diagnosis JSON on a pending attempt.
    /// Idempotent — calling twice overwrites. Returns the updated row;
    /// `Ok(None)` when the id is missing.
    pub fn set_conflict_resolution_diagnosis(
        &self,
        attempt_id: &str,
        diagnosis_json: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET conflict_diagnosis = ?2
              WHERE id = ?1",
            params![attempt_id, diagnosis_json],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Flip a `pending` attempt to `running` and stamp the lease
    /// triple (`cube_lease_id`, `cube_workspace_id`, `worker_id`) the
    /// coordinator just acquired. Idempotent — a second call with the
    /// same triple is a no-op. Returns the updated row.
    pub fn mark_conflict_resolution_running(
        &self,
        attempt_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        worker_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
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
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Worker-visible terminal transition: flip an attempt to
    /// `failed` with a reason. The Boss-tier `boss engine conflicts
    /// mark-failed` CLI lands here. `Ok(None)` when the id is unknown
    /// or already terminal.
    ///
    /// The companion success path is part of the auto-retire flow
    /// elsewhere; this method intentionally only handles the failure
    /// signal a worker emits when it hits a stop condition.
    pub fn mark_conflict_resolution_failed(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
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
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Auto-retire transition: flip an attempt from `pending` or `running`
    /// to `succeeded`, stamping `head_sha_after` if known and a fresh
    /// `finished_at`. Idempotent — a second call with the row already
    /// terminal returns `Ok(None)` and writes nothing. Phase 4 / design
    /// Q5: invoked by the merge poller's `on_resolved` path when
    /// GitHub reports the PR mergeable again.  Accepting `pending` covers
    /// the case where the PR becomes clean again before the worker starts.
    pub fn mark_conflict_resolution_succeeded(
        &self,
        attempt_id: &str,
        head_sha_after: Option<&str>,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
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
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Engine-side abandon: flip a non-terminal attempt to `abandoned`
    /// with the provided reason. Used for "we stepped away on purpose"
    /// terminations (parent PR closed, parent merged externally,
    /// manual override) where `failed` would be misleading. Idempotent.
    pub fn mark_conflict_resolution_abandoned(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
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
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Read-only list of `conflict_resolutions` rows for the Phase 5
    /// `boss engine conflicts list` CLI. Filters are AND-ed; an empty
    /// `status` slice means "any status." Rows come back freshest first
    /// (`created_at DESC, id DESC`) so the CLI's first row is the row a
    /// human typically wants. `limit = None` returns every match — the
    /// CLI caps with `--limit`, so the engine doesn't apply a default.
    pub fn list_conflict_resolutions(
        &self,
        product_id: Option<&str>,
        statuses: &[String],
        work_item_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<ConflictResolution>> {
        let conn = self.connect()?;
        let mut sql = String::from(
            "SELECT id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                    base_sha_at_trigger, head_sha_before, head_sha_after, status, failure_reason,
                    cube_lease_id, cube_workspace_id, worker_id, conflict_diagnosis,
                    created_at, started_at, finished_at
             FROM conflict_resolutions WHERE 1=1",
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
        let refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map_conflict_resolution)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Reset a terminal-failure attempt back to `pending` so the
    /// dispatcher re-spawns a worker. Only valid when the row's current
    /// status is `failed` or `abandoned`; the caller (CLI) is
    /// responsible for surfacing the rejection on a non-terminal row.
    ///
    /// The reset clears `failure_reason`, `head_sha_after`, the lease
    /// triple (`cube_lease_id`, `cube_workspace_id`, `worker_id`), and
    /// `finished_at`/`started_at` — i.e. it puts the row back into the
    /// shape the dispatcher expects for a fresh pending attempt. The
    /// parent work item is also re-flipped to `blocked: merge_conflict`
    /// (if currently `in_review`) and its `blocked_attempt_id` is
    /// repointed at the reset row. Returns the reset row on success;
    /// `Ok(None)` when the id is unknown or the row is non-terminal.
    pub fn retry_conflict_resolution(
        &self,
        attempt_id: &str,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE conflict_resolutions
                SET status            = 'pending',
                    failure_reason    = NULL,
                    head_sha_after    = NULL,
                    cube_lease_id     = NULL,
                    cube_workspace_id = NULL,
                    worker_id         = NULL,
                    started_at        = NULL,
                    finished_at       = NULL
              WHERE id = ?1
                AND status IN ('failed', 'abandoned')",
            params![attempt_id],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let reset = query_conflict_resolution(&tx, attempt_id)?
            .with_context(|| format!("unknown conflict_resolution after retry: {attempt_id}"))?;
        // Re-stamp the parent's blocked state so the kanban shows the
        // card in `blocked: merge_conflict` again, and so the dispatcher
        // re-picks the row up. The flip is best-effort — if the parent
        // is already `blocked: merge_conflict` (or has been moved
        // somewhere unexpected by a human), we leave it alone.
        tx.execute(
            "UPDATE tasks
                SET status             = 'blocked',
                    blocked_reason     = 'merge_conflict',
                    blocked_attempt_id = ?2,
                    last_status_actor  = 'engine',
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'in_review'
                AND pr_url = ?4
                AND deleted_at IS NULL",
            params![reset.work_item_id, reset.id, now, reset.pr_url],
        )?;
        // If the parent is already blocked: merge_conflict (e.g. the
        // retire path hasn't run because the conflict is still live),
        // just re-point the attempt id.
        tx.execute(
            "UPDATE tasks
                SET blocked_attempt_id = ?2,
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'blocked'
                AND blocked_reason = 'merge_conflict'
                AND deleted_at IS NULL",
            params![reset.work_item_id, reset.id, now],
        )?;
        tx.commit()?;
        Ok(Some(reset))
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

    /// Append an `effort_escalations` row recording a worker's
    /// `[effort-escalation]` Stop-boundary signal (design §Q5). The
    /// engine assigns `id` (prefix `esc_…`) and `created_at`.
    /// `markers` is stored as a JSON array; the audit report
    /// re-parses on read. Returns the inserted row wire-shape so
    /// the RPC caller can echo it back without a re-query.
    ///
    /// Validates that `work_item_id` refers to a known leaf row
    /// (chore / project_task / design) and resolves `product_id`
    /// from it; the denormalised `product_id` column avoids a join
    /// on every audit-report read.
    pub fn record_effort_escalation(
        &self,
        work_item_id: &str,
        original_level: boss_protocol::EffortLevel,
        new_level: boss_protocol::EffortLevel,
        markers: &[String],
        rule_id: Option<&str>,
    ) -> Result<boss_protocol::EffortEscalation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let product_id = product_id_for_work_item(&tx, work_item_id)
            .with_context(|| format!("unknown work item: {work_item_id}"))?;
        let id = next_id("esc");
        let now = now_string();
        let markers_json = serde_json::to_string(markers)
            .context("serialise effort escalation markers")?;
        tx.execute(
            "INSERT INTO effort_escalations
                 (id, product_id, work_item_id, original_level, new_level, markers, rule_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                product_id,
                work_item_id,
                original_level.as_str(),
                new_level.as_str(),
                markers_json,
                rule_id,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(boss_protocol::EffortEscalation {
            id,
            product_id,
            work_item_id: work_item_id.to_owned(),
            original_level,
            new_level,
            markers: markers.to_vec(),
            rule_id: rule_id.map(|s| s.to_owned()),
            created_at: now,
        })
    }

    /// Load every `effort_escalations` row for `product_id`,
    /// optionally filtered to events with `created_at >=
    /// since_epoch_secs`. Order is newest-first by `created_at`.
    /// Used by the audit report (design §Q4 follow-up).
    pub fn list_effort_escalations_for_product(
        &self,
        product_id: &str,
        since_epoch_secs: Option<i64>,
    ) -> Result<Vec<boss_protocol::EffortEscalation>> {
        let conn = self.connect()?;
        let mut sql = String::from(
            "SELECT id, product_id, work_item_id, original_level, new_level, markers, rule_id, created_at
             FROM effort_escalations
             WHERE product_id = ?1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(product_id.to_owned())];
        if let Some(since) = since_epoch_secs {
            sql.push_str(" AND CAST(created_at AS INTEGER) >= ?");
            params_vec.push(Box::new(since));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(refs.as_slice(), map_effort_escalation)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Project `(name, description)` for every active chore on
    /// `product_id`. Used by the audit report to compute the
    /// per-marker `matches` denominator. Excludes deleted rows and
    /// non-chore kinds — the audit is a per-product chore-corpus
    /// snapshot, not a cross-kind scan.
    pub fn list_chores_for_audit(
        &self,
        product_id: &str,
    ) -> Result<Vec<crate::audit_effort::ChoreForAudit>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT name, description
             FROM tasks
             WHERE product_id = ?1
               AND kind = 'chore'
               AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([product_id], |row| {
            Ok(crate::audit_effort::ChoreForAudit {
                name: row.get(0)?,
                description: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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

    // ── External-ref methods (T8) ────────────────────────────────────────────
    // Design: tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md
    // §"Design Question 4" and §"Lookup methods on WorkDb".

    /// Bind `work_item_id` to the upstream issue identified by `(kind,
    /// canonical_id)`. Stores the tracker-specific `raw` blob (e.g.
    /// `{"issue_number": 560, "project_item_id": "..."}` for GitHub).
    /// Clears any prior `external_ref_unbound_at` marker so the row is
    /// treated as actively bound. Replaces an existing binding silently.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn set_external_ref(
        &self,
        work_item_id: &str,
        kind: &str,
        canonical_id: &str,
        raw: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.connect()?;
        let raw_json = serde_json::to_string(raw)
            .with_context(|| format!("failed to serialise raw blob for {work_item_id}"))?;
        let n = conn.execute(
            "UPDATE tasks
             SET external_ref_kind         = ?2,
                 external_ref_canonical_id = ?3,
                 external_ref_raw          = ?4,
                 external_ref_unbound_at   = NULL,
                 updated_at                = ?5
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, kind, canonical_id, raw_json, now_string()],
        )?;
        if n == 0 {
            bail!("work item not found or soft-deleted: {work_item_id}");
        }
        Ok(())
    }

    /// Mark the external-ref binding on `work_item_id` as unbound.
    /// Retains `external_ref_kind` and `external_ref_canonical_id` so
    /// [`find_by_external_ref`][Self::find_by_external_ref] can
    /// re-bind automatically when the upstream item reappears. Sets
    /// `external_ref_unbound_at` to now and clears `external_ref_synced_at`.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn clear_external_ref(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE tasks
             SET external_ref_synced_at  = NULL,
                 external_ref_unbound_at = ?2,
                 updated_at              = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        if n == 0 {
            bail!("work item not found or soft-deleted: {work_item_id}");
        }
        Ok(())
    }

    /// Fetch a single task/chore by primary id, including the
    /// `external_ref_*` columns. Used by the `LinkWorkItemExternalRef` /
    /// `UnlinkWorkItemExternalRef` handlers so the `WorkItemUpdated`
    /// response carries the live `external_ref` snapshot.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn get_task_with_external_ref(&self, id: &str) -> Result<WorkItem> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state,
                    external_ref_kind, external_ref_canonical_id, external_ref_raw,
                    external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE id = ?1 AND deleted_at IS NULL",
            [id],
            map_task_with_external_ref,
        )
        .optional()
        .map_err(anyhow::Error::from)?
        .map(task_to_item)
        .with_context(|| format!("work item not found or soft-deleted: {id}"))
    }

    /// Find the work item actively bound to `(kind, canonical_id)`.
    /// Returns `None` when no matching active binding exists. Rows where
    /// `external_ref_unbound_at IS NOT NULL` are excluded (they retain
    /// their `canonical_id` for automatic re-binding, but are not
    /// considered "found" by this query). Soft-deleted tasks are always
    /// excluded.
    ///
    /// The returned `Task.external_ref` is populated; `web_url` is left
    /// as an empty string — derivation is tracker-specific and handled by
    /// the reconciler layer (T9).
    pub fn find_by_external_ref(
        &self,
        kind: &str,
        canonical_id: &str,
    ) -> Result<Option<Task>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state,
                    external_ref_kind, external_ref_canonical_id, external_ref_raw,
                    external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE external_ref_kind          = ?1
               AND external_ref_canonical_id  = ?2
               AND external_ref_unbound_at   IS NULL
               AND deleted_at               IS NULL",
            params![kind, canonical_id],
            map_task_with_external_ref,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return every task under `product_id` that has a non-null
    /// `external_ref_canonical_id`, including previously-unbound rows
    /// (where `external_ref_unbound_at IS NOT NULL`). The reconciler
    /// uses this list to detect reappearing items (and re-bind them via
    /// [`set_external_ref`][Self::set_external_ref]) as well as to build
    /// the canonical-id → work-item map for each reconcile pass.
    ///
    /// Soft-deleted tasks are excluded.
    pub fn list_external_refs_for_product(
        &self,
        product_id: &str,
    ) -> Result<Vec<(String, StoredExternalRef)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, external_ref_kind, external_ref_canonical_id,
                    external_ref_raw, external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE product_id                = ?1
               AND external_ref_canonical_id IS NOT NULL
               AND deleted_at               IS NULL",
        )?;
        let rows = stmt.query_map([product_id], |row| {
            let raw_json: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                raw_json,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (id, kind, canonical_id, raw_json, synced_at, unbound_at) = row?;
            let raw: serde_json::Value = raw_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null);
            result.push((
                id,
                StoredExternalRef {
                    kind,
                    canonical_id,
                    raw,
                    synced_at,
                    unbound_at,
                },
            ));
        }
        Ok(result)
    }

    /// Bump `external_ref_synced_at` to the current time for a work item.
    /// Called by the reconciler on every successful tick regardless of whether
    /// any other column changed. Does NOT update `updated_at` (keeping the
    /// reconciler tick invisible in the general-purpose "last modified" timeline).
    pub fn touch_external_ref_synced_at(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE tasks SET external_ref_synced_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        Ok(())
    }

    /// Move `work_item_id` to `status = 'done'`, clearing any block reason.
    /// No-op (returns `false`) when the row is already done/archived or soft-deleted.
    /// Used by the external-tracker reconciler for close-mirror (Behavior 2) and
    /// PR-merge-close (Behavior 5). Cascades the dep-unblock sweep after commit.
    pub fn reconciler_close_work_item(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(task) = query_task(&tx, work_item_id)? else {
            return Ok(false);
        };
        if task.deleted_at.is_some()
            || task.status == "done"
            || task.status == "archived"
        {
            return Ok(false);
        }
        let now = now_string();
        let n = tx.execute(
            "UPDATE tasks
             SET status             = 'done',
                 updated_at         = ?2,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1
               AND status NOT IN ('done', 'archived')
               AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        if n > 0 {
            cascade_dependents_after_prereq_status_change(&tx, work_item_id, "done", &now)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Set `pr_url` on a work item if it is currently `NULL` or empty.
    /// Returns `true` when the column was written, `false` when it was
    /// already set (preserving the existing URL, which may come from a
    /// more-trusted source like the `pr_url_capture` pipeline).
    pub fn reconciler_attach_pr_url(&self, work_item_id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE tasks
             SET pr_url = ?2, updated_at = ?3
             WHERE id = ?1
               AND deleted_at IS NULL
               AND (pr_url IS NULL OR pr_url = '')",
            params![work_item_id, pr_url, now],
        )?;
        Ok(n > 0)
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

/// Outcome of [`WorkDb::set_run_transcript_path_if_unset`]. The third
/// variant exists to keep "the latest run for this execution already
/// has a transcript_path" (legitimate no-op) distinguishable from
/// "no `work_runs` row exists for this execution yet" (real problem,
/// either a startup race or a wrong-namespace identifier). Returning
/// a flat `bool` from this call is what hid the 2026-05-12 bug:
/// every hook delivery silently looked like an already-set no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRunTranscriptPathOutcome {
    Updated,
    AlreadySet,
    RowMissing,
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

/// Raw external-ref data as stored in the `tasks` table. Returned by
/// [`WorkDb::list_external_refs_for_product`]. The `web_url` field present
/// on [`WorkItemExternalRef`] is tracker-specific and is derived by the
/// reconciler layer; the DB layer does not compute it.
#[derive(Debug, Clone)]
pub struct StoredExternalRef {
    pub kind: String,
    pub canonical_id: String,
    pub raw: serde_json::Value,
    pub synced_at: Option<String>,
    pub unbound_at: Option<String>,
}

/// A `conflict_resolutions` row that is `pending` but has no live
/// execution (`kind='conflict_resolution'` with status in
/// `'ready'`, `'running'`, or `'waiting_human'`). The merge poller's
/// stranded-attempt sweep rescues these by re-emitting a fresh
/// execution request.
///
/// `abandoned` rows are excluded by the caller's SQL (`status =
/// 'pending'` filter) — the churn guard or a human owns that path.
#[derive(Debug, Clone)]
pub struct StrandedConflictAttempt {
    pub attempt_id: String,
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
    let external_tracker_kind: Option<String> =
        row.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty());
    let external_tracker_config: Option<serde_json::Value> = row
        .get::<_, Option<String>>(11)?
        .and_then(|s| serde_json::from_str(&s).ok());
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        repo_remote_url: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        default_model: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
        dispatch_preamble: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
        external_tracker_kind,
        external_tracker_config,
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
        design_doc_repo_remote_url: row.get(11)?,
        design_doc_branch: row.get(12)?,
        design_doc_path: row.get(13)?,
        short_id: row.get(14)?,
    })
}

fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let effort_raw: Option<String> = row.get(19)?;
    let effort_level = match effort_raw.as_deref() {
        None | Some("") => None,
        Some(s) => match s.parse::<EffortLevel>() {
            Ok(level) => Some(level),
            Err(err) => {
                // The column is constrained in code, not by SQL. A row
                // carrying an out-of-set value is engine-side data
                // corruption: surface it loudly rather than silently
                // dropping the level.
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    19,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
                ));
            }
        },
    };
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
        created_via: row.get(15)?,
        blocked_reason: row.get(16)?,
        blocked_attempt_id: row.get(17)?,
        repo_remote_url: row.get(18)?,
        effort_level,
        model_override: row.get::<_, Option<String>>(20)?.filter(|s| !s.is_empty()),
        ci_attempt_budget: row.get(21)?,
        ci_attempts_used: row.get(22)?,
        short_id: row.get(23)?,
        // The multi-signal projection is built from the
        // `task_blocked_signals` side table by the engine's signal-
        // aggregation path (`merge-conflict-handling-in-review.md` §Q2),
        // which lands in a later phase. Until then the wire field is
        // always empty; consumers fall back to the scalar
        // `blocked_reason` / `blocked_attempt_id` cache above.
        blocked_signals: Vec::new(),
        ci_required_state: row.get::<_, Option<String>>(24)?.filter(|s| !s.is_empty()),
        review_required_state: row.get::<_, Option<String>>(25)?.filter(|s| !s.is_empty()),
        ci_required_detail: row.get::<_, Option<String>>(26)?.filter(|s| !s.is_empty()),
        review_required_detail: row.get::<_, Option<String>>(27)?.filter(|s| !s.is_empty()),
        pr_state_polled_at: row.get::<_, Option<String>>(28)?.filter(|s| !s.is_empty()),
        merge_queue_state: row.get::<_, Option<String>>(29)?.filter(|s| !s.is_empty()),
        // Standard queries omit the external_ref columns; the T8 methods
        // use map_task_with_external_ref which adds columns 30-34.
        // T1 schema columns; populated by T8 WorkDb methods when the migration
        // has run. Until then the protocol field carries None.
        external_ref: None,
    })
}

/// Like [`map_task`] but reads columns 30–34 carrying the external-ref
/// data and populates `Task.external_ref`. Used by the T8 WorkDb
/// methods (`find_by_external_ref`) whose SELECT explicitly includes
/// those columns. The `web_url` field is not stored in the DB; it is
/// derived at the reconciler layer and left as an empty string here.
fn map_task_with_external_ref(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    let kind: Option<String> = row.get(30)?;
    let canonical_id: Option<String> = row.get(31)?;
    if let (Some(kind), Some(canonical_id)) = (kind, canonical_id) {
        let raw_json: Option<String> = row.get(32)?;
        let raw: serde_json::Value = raw_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        task.external_ref = Some(WorkItemExternalRef {
            kind,
            canonical_id,
            raw,
            web_url: String::new(),
            synced_at: row.get(33)?,
            unbound_at: row.get(34)?,
        });
    }
    Ok(task)
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
        pre_start_failure_count: row.get(14)?,
        dispatch_not_before: row.get(15)?,
        pr_url: row.get(16)?,
        pr_head_before: row.get(17)?,
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
        work_item_id: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        title: row.get(5)?,
        body_markdown: row.get(6)?,
        created_at: row.get(7)?,
        resolved_at: row.get(8)?,
    })
}

fn map_effort_escalation(row: &Row<'_>) -> rusqlite::Result<boss_protocol::EffortEscalation> {
    use std::str::FromStr;
    let id: String = row.get(0)?;
    let product_id: String = row.get(1)?;
    let work_item_id: String = row.get(2)?;
    let original_level_str: String = row.get(3)?;
    let new_level_str: String = row.get(4)?;
    let markers_json: String = row.get(5)?;
    let rule_id: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    // Both level columns and the markers JSON were validated at
    // insert time; on read we treat schema-level corruption as a
    // row-level error so an unexpected value doesn't silently
    // poison the audit.
    let original_level = boss_protocol::EffortLevel::from_str(&original_level_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into()))?;
    let new_level = boss_protocol::EffortLevel::from_str(&new_level_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into()))?;
    let markers: Vec<String> = serde_json::from_str(&markers_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into()))?;
    Ok(boss_protocol::EffortEscalation {
        id,
        product_id,
        work_item_id,
        original_level,
        new_level,
        markers,
        rule_id,
        created_at,
    })
}

fn map_conflict_resolution(row: &Row<'_>) -> rusqlite::Result<ConflictResolution> {
    Ok(ConflictResolution {
        id: row.get(0)?,
        product_id: row.get(1)?,
        work_item_id: row.get(2)?,
        pr_url: row.get(3)?,
        pr_number: row.get(4)?,
        head_branch: row.get(5)?,
        base_branch: row.get(6)?,
        base_sha_at_trigger: row.get(7)?,
        head_sha_before: row.get(8)?,
        head_sha_after: row.get(9)?,
        status: row.get(10)?,
        failure_reason: row.get(11)?,
        cube_lease_id: row.get(12)?,
        cube_workspace_id: row.get(13)?,
        worker_id: row.get(14)?,
        conflict_diagnosis: row.get(15)?,
        created_at: row.get(16)?,
        started_at: row.get(17)?,
        finished_at: row.get(18)?,
    })
}

fn query_conflict_resolution(
    conn: &Connection,
    id: &str,
) -> Result<Option<ConflictResolution>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                base_sha_at_trigger, head_sha_before, head_sha_after, status, failure_reason,
                cube_lease_id, cube_workspace_id, worker_id, conflict_diagnosis,
                created_at, started_at, finished_at
         FROM conflict_resolutions
         WHERE id = ?1",
    )?;
    let row = stmt
        .query_row([id], map_conflict_resolution)
        .optional()?;
    Ok(row)
}

/// Pre-insert payload for [`WorkDb::insert_conflict_resolution`].
/// Fields mirror the `conflict_resolutions` schema; everything the
/// engine knows at detection time is required, everything the engine
/// stamps post-spawn (`head_sha_after`, `cube_lease_id`, …) is
/// omitted.
#[derive(Debug, Clone)]
pub struct ConflictResolutionInsertInput {
    pub product_id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub base_branch: String,
    pub base_sha_at_trigger: Option<String>,
    pub head_sha_before: Option<String>,
}

/// Pre-insert payload for [`WorkDb::insert_ci_remediation`]. Mirrors
/// the `ci_remediations` schema for the engine-known fields at
/// detection time. `consumes_budget` is `1` for `attempt_kind='fix'`
/// and `0` for `'retrigger'` per design §Q3.
#[derive(Debug, Clone)]
pub struct CiRemediationInsertInput {
    pub product_id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub head_sha_at_trigger: String,
    pub attempt_kind: String,
    pub consumes_budget: i64,
    /// JSON-encoded list of failing-check snapshots captured at
    /// trigger time. The engine writes this on detection; the worker
    /// reads it via the spawned prompt.
    pub failed_checks: String,
}

fn map_ci_remediation(row: &Row<'_>) -> rusqlite::Result<CiRemediation> {
    Ok(CiRemediation {
        id: row.get(0)?,
        product_id: row.get(1)?,
        work_item_id: row.get(2)?,
        pr_url: row.get(3)?,
        pr_number: row.get(4)?,
        head_branch: row.get(5)?,
        head_sha_at_trigger: row.get(6)?,
        head_sha_after: row.get(7)?,
        attempt_kind: row.get(8)?,
        consumes_budget: row.get(9)?,
        failed_checks: row.get(10)?,
        triage_class: row.get(11)?,
        log_excerpt: row.get(12)?,
        status: row.get(13)?,
        failure_reason: row.get(14)?,
        cube_lease_id: row.get(15)?,
        cube_workspace_id: row.get(16)?,
        worker_id: row.get(17)?,
        created_at: row.get(18)?,
        started_at: row.get(19)?,
        finished_at: row.get(20)?,
    })
}

fn query_ci_remediation(conn: &Connection, id: &str) -> Result<Option<CiRemediation>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, work_item_id, pr_url, pr_number,
                head_branch, head_sha_at_trigger, head_sha_after,
                attempt_kind, consumes_budget, failed_checks,
                triage_class, log_excerpt, status, failure_reason,
                cube_lease_id, cube_workspace_id, worker_id,
                created_at, started_at, finished_at
         FROM ci_remediations
         WHERE id = ?1",
    )?;
    let row = stmt.query_row([id], map_ci_remediation).optional()?;
    Ok(row)
}

/// Upsert the multi-signal side table for a `(work_item_id, reason)`
/// pair. The PK collapses repeat observations to one row; we reset
/// `cleared_at` to NULL on re-observation so the same signal flapping
/// in and out lands as one row with the latest `created_at`.
///
/// `attempt_id` is the soft FK that the design's §Q2 stores so the UI
/// can navigate from a signal back to its attempt row; `None` for
/// `'dependency'` (which has no attempt table) and for the
/// `'ci_failure_exhausted'` signal (which is the *absence* of an
/// engine-managed attempt — the engine has stopped trying).
fn upsert_task_blocked_signal(
    conn: &Connection,
    work_item_id: &str,
    reason: &str,
    attempt_id: Option<&str>,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO task_blocked_signals
             (work_item_id, reason, attempt_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(work_item_id, reason) DO UPDATE SET
             attempt_id = COALESCE(excluded.attempt_id, task_blocked_signals.attempt_id),
             cleared_at = NULL",
        params![work_item_id, reason, attempt_id, now],
    )?;
    Ok(())
}

/// Check whether a non-deleted task/chore with the same trimmed name
/// exists in the same product and was created within `DUPLICATE_GUARD_WINDOW_SECS`.
/// Returns `Some(DuplicateTaskError)` when the guard fires, `None` otherwise.
fn check_recent_duplicate(
    conn: &Connection,
    product_id: &str,
    name: &str,
) -> Result<Option<DuplicateTaskError>> {
    let trimmed = name.trim();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cutoff = now_secs - DUPLICATE_GUARD_WINDOW_SECS;

    let row: Option<(String, Option<i64>, i64)> = conn
        .query_row(
            "SELECT id, short_id, CAST(created_at AS INTEGER)
             FROM tasks
             WHERE product_id = ?1
               AND trim(name) = ?2
               AND deleted_at IS NULL
               AND CAST(created_at AS INTEGER) >= ?3
             ORDER BY CAST(created_at AS INTEGER) DESC
             LIMIT 1",
            params![product_id, trimmed, cutoff],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    Ok(row.map(|(existing_id, existing_short_id, created_at)| DuplicateTaskError {
        existing_id,
        existing_short_id: existing_short_id.unwrap_or(0),
        name: trimmed.to_owned(),
        age_secs: now_secs - created_at,
    }))
}

fn insert_task_in_tx(conn: &Connection, input: CreateTaskInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    ensure_project_belongs_to_product(conn, &input.project_id, &input.product_id)?;

    if !input.force_duplicate {
        if let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)? {
            return Err(anyhow::Error::new(dup));
        }
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, &input.project_id)?;
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "task");
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, short_id)
         VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![id, input.product_id, input.project_id, input.name, description, ordinal, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, short_id],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing task after insert: {id}"))
}

fn insert_chore_in_tx(conn: &Connection, input: CreateChoreInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;

    if !input.force_duplicate {
        if let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)? {
            return Err(anyhow::Error::new(dup));
        }
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "chore");
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, short_id)
         VALUES (?1, ?2, NULL, 'chore', ?3, ?4, 'todo', NULL, NULL, NULL, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![id, input.product_id, input.name, description, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, short_id],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing chore after insert: {id}"))
}

/// Trim and reduce an empty model slug to `None`. The CLI uses
/// `--model ""` to clear a stored override on update verbs; the
/// engine treats the same shape consistently on create so callers
/// don't have to special-case empty strings. Non-empty strings pass
/// through verbatim — claude is the source of truth on slug
/// resolution (design §Q3).
fn normalize_model_override(raw: Option<String>) -> Option<String> {
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
fn insert_design_task_for_project_in_tx(
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
fn canonicalize_created_via(
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
        "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model, dispatch_preamble, external_tracker_kind, external_tracker_config
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
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
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
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
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
                created_at, started_at, finished_at,
                pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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
        "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
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
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
         FROM projects
         WHERE product_id = ?1
         ORDER BY created_at ASC, name COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([product_id], map_project)?;
    collect_rows(rows)
}

fn list_tasks_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state
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

fn migrate_work_executions_pre_start_retry(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "pre_start_failure_count",
            "ALTER TABLE work_executions ADD COLUMN pre_start_failure_count INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "dispatch_not_before",
            "ALTER TABLE work_executions ADD COLUMN dispatch_not_before TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

fn migrate_work_executions_pr_url(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_url")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN pr_url TEXT",
            [],
        )?;
    }
    Ok(())
}

/// `pr_head_before`: the head SHA of the chore's bound PR captured
/// at the moment this execution started running. The Stop boundary's
/// SHA-delta gate uses it to decide whether a resume run actually
/// contributed to the bound PR before falling through to the
/// `PROBE_NO_PR` nudge — see the resume-bounce nudge-loop fix.
/// Idempotent.
fn migrate_work_executions_pr_head_before(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_head_before")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN pr_head_before TEXT",
            [],
        )?;
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

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
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

/// Add the per-work-item `repo_remote_url` override to `tasks`. `NULL`
/// (the default for existing rows) means "inherit from the parent
/// product's `repo_remote_url`"; a non-`NULL` value wins the
/// resolution at dispatch time. Purely additive — see
/// `tools/boss/docs/designs/multi-repo-work-modeling.md` (Q1).
fn migrate_tasks_repo_remote_url(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "repo_remote_url")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN repo_remote_url TEXT", [])?;
    }
    Ok(())
}

/// Add `created_via` to `tasks` so the engine records the surface
/// that filed each chore/task — `cli`, `bossctl`, `mac_app`, or
/// `engine_auto`. Existing rows default to `unknown` (the same
/// fallback the engine uses when a caller omits the field). The
/// column is purely additive; no existing query depends on it.
fn migrate_tasks_created_via(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "created_via")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN created_via TEXT NOT NULL DEFAULT 'unknown'",
            [],
        )?;
    }
    Ok(())
}


/// Add per-project design-doc pointer columns. The three columns
/// jointly identify "where this project's design doc lives" and
/// are all nullable: `design_doc_path` is the load-bearing field
/// and a `NULL` path means no pointer is set. The other two are
/// optional overrides that fall back to the product's repo /
/// docs-branch defaults when `NULL`. Existing rows keep `NULL` on
/// all three across the upgrade.
fn migrate_project_design_doc_columns(conn: &Connection) -> Result<()> {
    for column in [
        "design_doc_repo_remote_url",
        "design_doc_branch",
        "design_doc_path",
    ] {
        if !table_has_column(conn, "projects", column)? {
            let ddl = format!("ALTER TABLE projects ADD COLUMN {column} TEXT");
            conn.execute(&ddl, [])?;
        }
    }
    Ok(())
}

/// Create the `project_property_audit` side table for the
/// design-doc-pointer audit log (chore #15 of the
/// `project-design-doc-pointer` design). Append-only history of
/// `projects.design_doc_*` writes, with one row per (column, write)
/// pair where the value actually changed.
///
/// `project_id` is intentionally *not* a foreign key — projects can
/// be soft-deleted out from under their history, but the forensic
/// goal of the table is to survive that. The index keyed on
/// `(project_id, changed_at)` covers the only read pattern v1 ships
/// (list-by-project, chronological).
fn migrate_project_property_audit_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_property_audit (
             id          TEXT PRIMARY KEY,
             project_id  TEXT NOT NULL,
             property    TEXT NOT NULL,
             old_value   TEXT,
             new_value   TEXT,
             actor       TEXT NOT NULL,
             changed_at  TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS project_property_audit_project_idx
             ON project_property_audit(project_id, changed_at);",
    )?;
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
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
             VALUES (?1, ?2, ?3, 'design', 'Design', '', 'todo', 0, NULL, NULL, ?4, ?4, 0, 'medium', ?5)",
            params![id, product_id, project_id, now, CREATED_VIA_ENGINE_AUTO],
        )?;
    }
    Ok(())
}

/// Add `blocked_reason` and `blocked_attempt_id` columns on `tasks`.
/// `blocked_reason` discriminates *why* a row is in `status = 'blocked'`
/// (`'dependency'` for the existing dep-graph machinery,
/// `'merge_conflict'` for the conflict-resolution flow, `'review_feedback'`
/// for the review-iteration flow, etc.). `blocked_attempt_id` is a soft
/// FK whose target table is discriminated by `blocked_reason` — `NULL`
/// for `'dependency'`, points at a `conflict_resolutions.id` for
/// `'merge_conflict'`. Both columns are nullable: legacy `blocked` rows
/// without a recoverable reason stay `NULL`.
fn migrate_tasks_blocked_reason(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "blocked_reason")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN blocked_reason TEXT", [])?;
    }
    if !table_has_column(conn, "tasks", "blocked_attempt_id")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN blocked_attempt_id TEXT", [])?;
    }
    Ok(())
}

/// Add `products.auto_pr_maintenance_enabled` — the unified opt-out
/// flag governing every auto-PR-maintenance flow (auto-rebase,
/// conflict resolution, CI remediation). Defaults to `1` (enabled).
///
/// Backwards-compat path: if a previous build of this codebase already
/// shipped `products.auto_rebase_enabled` (the original auto-rebase
/// design's flag), rename it in place to the new name so the existing
/// value carries over. If neither column exists, create the new one
/// directly. Both branches are idempotent.
fn migrate_products_auto_pr_maintenance_enabled(conn: &Connection) -> Result<()> {
    let has_old = table_has_column(conn, "products", "auto_rebase_enabled")?;
    let has_new = table_has_column(conn, "products", "auto_pr_maintenance_enabled")?;
    if has_new {
        return Ok(());
    }
    if has_old {
        conn.execute(
            "ALTER TABLE products RENAME COLUMN auto_rebase_enabled TO auto_pr_maintenance_enabled",
            [],
        )?;
    } else {
        conn.execute(
            "ALTER TABLE products ADD COLUMN auto_pr_maintenance_enabled INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    Ok(())
}

/// Create the `conflict_resolutions` side table. Stores one row per
/// engine attempt to clear a merge conflict on an in-review PR; rows
/// are sparse (most PRs never conflict) and retained after success as
/// history. See `tools/boss/docs/designs/merge-conflict-handling-in-review.md`
/// (Q3) for the rationale on why this is a side table rather than a
/// `tasks` row.
fn migrate_conflict_resolutions_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conflict_resolutions (
             id                  TEXT PRIMARY KEY,
             product_id          TEXT NOT NULL,
             work_item_id        TEXT NOT NULL,
             pr_url              TEXT NOT NULL,
             pr_number           INTEGER NOT NULL,
             head_branch         TEXT NOT NULL,
             base_branch         TEXT NOT NULL,
             base_sha_at_trigger TEXT,
             head_sha_before     TEXT,
             head_sha_after      TEXT,
             status              TEXT NOT NULL,
             failure_reason      TEXT,
             cube_lease_id       TEXT,
             cube_workspace_id   TEXT,
             worker_id           TEXT,
             conflict_diagnosis  TEXT,
             created_at          TEXT NOT NULL,
             started_at          TEXT,
             finished_at         TEXT,
             UNIQUE (work_item_id, base_sha_at_trigger)
         );
         CREATE INDEX IF NOT EXISTS conflict_resolutions_status_idx
             ON conflict_resolutions(status);
         CREATE INDEX IF NOT EXISTS conflict_resolutions_work_item_idx
             ON conflict_resolutions(work_item_id);
         CREATE INDEX IF NOT EXISTS conflict_resolutions_product_idx
             ON conflict_resolutions(product_id);",
    )?;
    Ok(())
}

/// Backfill `blocked_reason = 'dependency'` for `blocked` rows that
/// have at least one currently-gating prerequisite edge. The dep-graph
/// machinery owns the `'dependency'` reason going forward; this pass
/// catches rows the dep-graph machinery flipped before the column
/// existed. Rows that remain `blocked` with no gating prereq stay
/// `NULL` (legacy "blocked by a human for some untracked reason").
/// Idempotent — the `blocked_reason IS NULL` guard means re-running
/// the migration is a no-op once values are written.
/// Schema v7: relax `work_attention_items.execution_id` to nullable
/// and add a `work_item_id` column so an attention item can attach to
/// a work item that has no execution row yet (`repo_unresolved` per
/// `multi-repo-work-modeling.md` Q5). SQLite cannot drop a `NOT NULL`
/// constraint in place, so we rebuild the table.
///
/// Idempotent: the table rebuild is guarded by the presence of the
/// new column. The index DDL is `IF NOT EXISTS` and runs every time
/// so fresh-init databases (which create the table directly in its
/// v7 shape) also pick up the index.
fn migrate_work_attention_items_work_item_id(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "work_attention_items", "work_item_id")? {
        conn.execute_batch(
            "CREATE TABLE work_attention_items_v7 (
                 id TEXT PRIMARY KEY,
                 execution_id TEXT REFERENCES work_executions(id) ON DELETE CASCADE,
                 work_item_id TEXT,
                 kind TEXT NOT NULL,
                 status TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body_markdown TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 resolved_at TEXT,
                 CHECK (
                     (execution_id IS NOT NULL AND work_item_id IS NULL)
                     OR (execution_id IS NULL AND work_item_id IS NOT NULL)
                 )
             );
             INSERT INTO work_attention_items_v7
                 (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
             SELECT id, execution_id, NULL, kind, status, title, body_markdown, created_at, resolved_at
                 FROM work_attention_items;
             DROP TABLE work_attention_items;
             ALTER TABLE work_attention_items_v7 RENAME TO work_attention_items;
             CREATE INDEX IF NOT EXISTS work_attention_items_execution_idx
                 ON work_attention_items(execution_id, created_at);",
        )?;
    }
    // Index DDL runs unconditionally — the table is always v7-shaped
    // by this point, and `IF NOT EXISTS` makes it idempotent. Fresh
    // init lands here too (the new-shape `CREATE TABLE IF NOT EXISTS`
    // creates the table but not this column-specific index).
    conn.execute(
        "CREATE INDEX IF NOT EXISTS work_attention_items_work_item_idx
            ON work_attention_items(work_item_id, created_at)",
        [],
    )?;
    Ok(())
}

/// Add `tasks.effort_level` and `tasks.model_override` per the
/// effort-and-model-estimation design (PR #370). Both columns are
/// nullable TEXT; existing rows keep `NULL` across the upgrade so
/// dispatcher behaviour is unchanged for unset rows (Q3 step 4).
///
/// `effort_level` is constrained in code (see [`EffortLevel`]); we
/// deliberately do NOT add a SQL `CHECK` — the rule lives in the
/// engine and bumping the enum should never require a schema rebuild.
/// `model_override` carries a Claude model slug verbatim — also
/// unvalidated at write time so a new model can ship without an
/// engine release blocking adoption (design §Q3).
fn migrate_tasks_effort_and_model_columns(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "effort_level")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN effort_level TEXT", [])?;
    }
    if !table_has_column(conn, "tasks", "model_override")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN model_override TEXT", [])?;
    }
    Ok(())
}

/// Add `products.default_model` per the effort-and-model-estimation
/// design (PR #370). Nullable TEXT carrying a Claude model slug
/// verbatim; existing product rows keep `NULL`. Lets a product owner
/// set "default everything on this product to Sonnet" without
/// touching every row's `model_override` (design §Q3 precedence step
/// 3).
fn migrate_products_default_model(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "default_model")? {
        conn.execute("ALTER TABLE products ADD COLUMN default_model TEXT", [])?;
    }
    Ok(())
}

fn migrate_backfill_blocked_reason_dependency(conn: &Connection) -> Result<()> {
    // The dep-graph machinery defines "gating" as a `relation = 'blocks'`
    // edge whose prereq has not reached a satisfied terminal status. For
    // task/chore prereqs (`task_…`) only `'done'` satisfies; for project
    // prereqs (`proj_…`) `'done'` or `'archived'` satisfies. SQL mirrors
    // `work_dependencies::status_satisfies` exactly.
    conn.execute(
        "UPDATE tasks
            SET blocked_reason = 'dependency'
          WHERE status = 'blocked'
            AND blocked_reason IS NULL
            AND deleted_at IS NULL
            AND EXISTS (
              SELECT 1
                FROM work_item_dependencies d
                LEFT JOIN tasks    pt ON pt.id = d.prerequisite_id AND pt.deleted_at IS NULL
                LEFT JOIN projects pp ON pp.id = d.prerequisite_id
               WHERE d.dependent_id = tasks.id
                 AND d.relation = 'blocks'
                 AND (
                   (pt.id IS NOT NULL AND pt.status <> 'done')
                   OR (pp.id IS NOT NULL AND pp.status <> 'done' AND pp.status <> 'archived')
                 )
            )",
        [],
    )?;
    Ok(())
}

/// Create the `task_blocked_signals` side table — the multi-signal
/// companion to the scalar `tasks.blocked_reason` cache. One row per
/// active blocked-reason for a work item; the `(work_item_id, reason)`
/// PK doubles as the idempotency lock so re-observing the same signal
/// is an upsert rather than a duplicate row. `cleared_at` retains
/// history (alongside `conflict_resolutions` and `ci_remediations`).
/// See `merge-conflict-handling-in-review.md` §Q2 for rationale.
fn migrate_task_blocked_signals_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_blocked_signals (
             work_item_id  TEXT NOT NULL,
             reason        TEXT NOT NULL,
             attempt_id    TEXT,
             created_at    TEXT NOT NULL,
             cleared_at    TEXT,
             PRIMARY KEY (work_item_id, reason)
         );
         CREATE INDEX IF NOT EXISTS task_blocked_signals_active_idx
             ON task_blocked_signals(work_item_id, reason)
             WHERE cleared_at IS NULL;",
    )?;
    Ok(())
}

/// Create the `ci_remediations` side table — parallel to
/// `conflict_resolutions`, one row per engine attempt to clear a CI
/// failure on an in-review PR. Unique key
/// `(work_item_id, head_sha_at_trigger, attempt_kind)` keeps a
/// re-trigger and a fix on the same failing head sha distinct while
/// still locking out duplicate probes for the same triplet. See
/// `merge-conflict-handling-in-review.md` §Q3 for the side-table-not-
/// tasks-row rationale and the per-PR-not-per-failure budget choice.
fn migrate_ci_remediations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ci_remediations (
             id                  TEXT PRIMARY KEY,
             product_id          TEXT NOT NULL,
             work_item_id        TEXT NOT NULL,
             pr_url              TEXT NOT NULL,
             pr_number           INTEGER NOT NULL,
             head_branch         TEXT NOT NULL,
             head_sha_at_trigger TEXT NOT NULL,
             head_sha_after      TEXT,
             attempt_kind        TEXT NOT NULL,
             consumes_budget     INTEGER NOT NULL,
             failed_checks       TEXT NOT NULL,
             triage_class        TEXT,
             log_excerpt         TEXT,
             status              TEXT NOT NULL,
             failure_reason      TEXT,
             cube_lease_id       TEXT,
             cube_workspace_id   TEXT,
             worker_id           TEXT,
             created_at          TEXT NOT NULL,
             started_at          TEXT,
             finished_at         TEXT,
             UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)
         );
         CREATE INDEX IF NOT EXISTS ci_remediations_status_idx
             ON ci_remediations(status);
         CREATE INDEX IF NOT EXISTS ci_remediations_work_item_idx
             ON ci_remediations(work_item_id);
         CREATE INDEX IF NOT EXISTS ci_remediations_product_idx
             ON ci_remediations(product_id);",
    )?;
    Ok(())
}

/// Create the `ci_failure_suppressions` table — the thin escape
/// hatch consulted by `ci_watch::on_ci_failure_detected` when the
/// user has manually moved a chore out of `blocked: ci_failure`. A
/// row pins suppression for one `(work_item, head_sha)` pair; a new
/// head sha invalidates it automatically. See
/// `merge-conflict-handling-in-review.md` §Q5 ("Manual override
/// (CI)") for the lifecycle.
fn migrate_ci_failure_suppressions_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ci_failure_suppressions (
             work_item_id  TEXT NOT NULL,
             head_sha      TEXT NOT NULL,
             created_at    TEXT NOT NULL,
             PRIMARY KEY (work_item_id, head_sha)
         );",
    )?;
    Ok(())
}

/// Add `tasks.ci_attempt_budget` (per-PR override, NULL = inherit
/// the product default) and `tasks.ci_attempts_used` (counter,
/// default 0). Existing rows pick up NULL / 0 — the budget kicks in
/// only when the parent enters the CI-failure flow, so legacy
/// in-flight PRs are unaffected until they next go red. See
/// `merge-conflict-handling-in-review.md` §Q3 for the reset rules
/// and the "what counts as one attempt" definition.
fn migrate_tasks_ci_attempt_columns(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "ci_attempt_budget")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN ci_attempt_budget INTEGER", [])?;
    }
    if !table_has_column(conn, "tasks", "ci_attempts_used")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN ci_attempts_used INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Add `products.ci_attempt_budget` — the product-level default the
/// engine falls back to when a task / chore has no per-PR
/// `tasks.ci_attempt_budget` set. Default 3 per design §Q3 ("Default
/// 3 attempts per PR"). Existing product rows inherit the default.
fn migrate_products_ci_attempt_budget(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "ci_attempt_budget")? {
        conn.execute(
            "ALTER TABLE products ADD COLUMN ci_attempt_budget INTEGER NOT NULL DEFAULT 3",
            [],
        )?;
    }
    Ok(())
}

/// Add `products.dispatch_preamble` — an optional text string prepended
/// (with a visible bracket marker) to every worker's initial context
/// at spawn time. `NULL` / empty → no injection (existing behaviour).
/// Lets a product owner set per-product runtime guidance (e.g. test-runner
/// preferences) that workers see on every spawn.
fn migrate_products_dispatch_preamble(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "dispatch_preamble")? {
        conn.execute("ALTER TABLE products ADD COLUMN dispatch_preamble TEXT", [])?;
    }
    Ok(())
}

/// Mirror existing `tasks.blocked_reason` scalars into the side
/// table so the multi-signal projection is internally consistent on
/// first open after the schema lands. The pre-Phase-7 invariant is
/// at most one reason per row, so a single INSERT-from-SELECT pass
/// is correct.
///
/// `attempt_id` carries through `tasks.blocked_attempt_id` (it is
/// the soft FK already discriminated by reason). `created_at` uses
/// the row's `updated_at` as a best-effort timestamp for when the
/// block was last touched — better than `NULL`, and the engine
/// re-stamps with `now()` on the next sweep that observes the
/// signal anyway.
///
/// Idempotent: re-running the migration after the first open is a
/// no-op because the existing rows already match the
/// `(work_item_id, reason)` PK (`INSERT OR IGNORE`).
fn migrate_backfill_task_blocked_signals(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO task_blocked_signals
             (work_item_id, reason, attempt_id, created_at, cleared_at)
         SELECT id, blocked_reason, blocked_attempt_id, updated_at, NULL
           FROM tasks
          WHERE blocked_reason IS NOT NULL
            AND status = 'blocked'
            AND deleted_at IS NULL",
        [],
    )?;
    Ok(())
}

/// Create the `effort_escalations` side table — one row per
/// `[effort-escalation]` Stop-boundary signal the coordinator
/// observed (design §Q5). The audit report (`boss product
/// audit-effort`, design §Q4 follow-up) reads this table; the
/// sibling escalation-handler task writes to it.
///
/// `original_level` / `new_level` are stored as TEXT to mirror
/// `tasks.effort_level` — same enum, same lack of CHECK
/// constraint, validated in code via
/// [`boss_protocol::EffortLevel::from_str`].
/// `markers` is a JSON-encoded array of strings (the §Q4 marker
/// list the heuristic matched against the row at creation), kept
/// in one column rather than a normalised side table because the
/// audit only ever scans events in bulk — the join cost would
/// outweigh the storage win.
fn migrate_effort_escalations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS effort_escalations (
             id             TEXT PRIMARY KEY,
             product_id     TEXT NOT NULL,
             work_item_id   TEXT NOT NULL,
             original_level TEXT NOT NULL,
             new_level      TEXT NOT NULL,
             markers        TEXT NOT NULL,
             rule_id        TEXT,
             created_at     TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS effort_escalations_product_idx
             ON effort_escalations(product_id, created_at);
         CREATE INDEX IF NOT EXISTS effort_escalations_work_item_idx
             ON effort_escalations(work_item_id);",
    )?;
    Ok(())
}

/// NULL out `tasks.repo_remote_url` where the override simply mirrors
/// the parent product's own repo. These rows were stamped incorrectly
/// by the creation-time resolver (which used to materialise the product
/// default into the task row instead of leaving it `NULL`).
///
/// Idempotent: rows already `NULL` are not touched; rows whose override
/// genuinely differs from their product (legitimate multi-repo task
/// overrides) are left unchanged.
fn migrate_null_redundant_task_repo_remote_urls(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE tasks
         SET repo_remote_url = NULL
         WHERE repo_remote_url IS NOT NULL
           AND id IN (
               SELECT t.id
               FROM tasks t
               JOIN products p ON p.id = t.product_id
               WHERE t.repo_remote_url IS NOT NULL
                 AND p.repo_remote_url IS NOT NULL
                 AND t.repo_remote_url = p.repo_remote_url
           )",
        [],
    )?;
    Ok(())
}

/// Add `short_id` columns to `tasks` and `projects`, the
/// `short_id_sequences` counter table, the per-product unique partial
/// indexes, and backfill existing rows per the design's Q4 rules
/// (`tools/boss/docs/designs/friendly-numeric-ids-for-work-items.md`).
///
/// Per-product across all kinds: for each product, the existing
/// `tasks` rows (every `kind`, including soft-deleted) and the
/// existing `projects` rows are merged into one stream, sorted by
/// `(created_at ASC, id ASC)`, and assigned `1..N`. The counter is
/// stamped at `N + 1` so the runtime allocator picks up where the
/// backfill stopped. The migration is idempotent — rows that already
/// have a `short_id` (e.g. a partial prior run, or a row inserted by
/// the runtime allocator before this migration somehow ran) are
/// skipped, and the counter is always advanced past the current
/// `MAX(short_id)` to keep the unique index happy.
fn migrate_short_id_columns(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "short_id")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN short_id INTEGER", [])?;
    }
    if !table_has_column(conn, "projects", "short_id")? {
        conn.execute("ALTER TABLE projects ADD COLUMN short_id INTEGER", [])?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );",
    )?;

    // Collect product ids first to keep the prepared statement out of
    // the way of subsequent writes.
    let product_ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM products")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for product_id in &product_ids {
        // Merged stream of unnumbered tasks + projects for this
        // product, sorted by epoch-seconds `created_at` then `id`.
        // `CAST(... AS INTEGER)` makes the migration robust to any
        // residual ISO-shaped timestamp that `migrate_timestamps_to_epoch`
        // didn't normalise (CAST yields 0 for non-numeric strings;
        // the `id` tiebreaker still produces a deterministic order
        // in that pathological case).
        let merged: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT kind_label, id FROM (
                     SELECT 'tasks'    AS kind_label, id, CAST(created_at AS INTEGER) AS ts
                     FROM tasks
                     WHERE product_id = ?1 AND short_id IS NULL
                     UNION ALL
                     SELECT 'projects' AS kind_label, id, CAST(created_at AS INTEGER) AS ts
                     FROM projects
                     WHERE product_id = ?1 AND short_id IS NULL
                 )
                 ORDER BY ts ASC, id ASC",
            )?;
            let rows = stmt.query_map([product_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        // Start past both the prior `next_value` (if some earlier
        // partial backfill stamped one) and `MAX(short_id)` (if any
        // rows were already numbered). This keeps the partial unique
        // index from rejecting the writes below.
        let prior_next: i64 = conn
            .query_row(
                "SELECT next_value FROM short_id_sequences WHERE product_id = ?1",
                [product_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(1);
        let max_existing: i64 = conn.query_row(
            "SELECT COALESCE(MAX(short_id), 0) FROM (
                 SELECT short_id FROM tasks
                 WHERE product_id = ?1 AND short_id IS NOT NULL
                 UNION ALL
                 SELECT short_id FROM projects
                 WHERE product_id = ?1 AND short_id IS NOT NULL
             )",
            [product_id],
            |row| row.get(0),
        )?;
        let mut next = prior_next.max(max_existing + 1);

        for (table, row_id) in &merged {
            let update_sql = match table.as_str() {
                "tasks" => "UPDATE tasks SET short_id = ?1 WHERE id = ?2",
                "projects" => "UPDATE projects SET short_id = ?1 WHERE id = ?2",
                other => bail!("unexpected short_id backfill table: {other}"),
            };
            conn.execute(update_sql, params![next, row_id])?;
            next += 1;
        }

        conn.execute(
            "INSERT INTO short_id_sequences(product_id, next_value) VALUES(?1, ?2)
             ON CONFLICT(product_id) DO UPDATE SET next_value = excluded.next_value",
            params![product_id, next],
        )?;
    }

    // Create indexes after the backfill so the unique-partial check
    // doesn't fail mid-migration on a transient duplicate (it would
    // not fail given the above logic, but ordering it this way also
    // matches the design's safety stance).
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS tasks_product_short_id_idx
             ON tasks(product_id, short_id) WHERE short_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS projects_product_short_id_idx
             ON projects(product_id, short_id) WHERE short_id IS NOT NULL;",
    )?;

    Ok(())
}

/// Backfill `autostart = 0` for tasks that are past their first Doing
/// transition (AI #2, Incident 001). From schema version 10 onward
/// `autostart` is single-shot: the engine clears it to `0` when a row
/// first enters `active` via `start_execution_run`. Rows that already
/// made that transition before this migration still carry `autostart = 1`
/// in the column, so we clear them here. Any row whose `status != 'todo'`
/// has been dispatched at least once and no longer needs the flag.
fn migrate_backfill_autostart_consumed(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET autostart = 0 WHERE autostart = 1 AND status != 'todo'",
        [],
    )?;
    Ok(())
}

/// Add `ci_required_state`, `review_required_state`, `ci_required_detail`,
/// `review_required_detail`, `pr_state_polled_at`, and `merge_queue_state`
/// columns to the `tasks` table. These are populated by the merge poller on
/// every Review-lane sweep and surfaced to the macOS kanban as CI, review,
/// and merging indicators with tooltips. Idempotent — guarded by
/// `tasks_has_column`.
fn migrate_pr_poll_state_columns(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "ci_required_state",
            "ALTER TABLE tasks ADD COLUMN ci_required_state TEXT",
        ),
        (
            "review_required_state",
            "ALTER TABLE tasks ADD COLUMN review_required_state TEXT",
        ),
        (
            "ci_required_detail",
            "ALTER TABLE tasks ADD COLUMN ci_required_detail TEXT",
        ),
        (
            "review_required_detail",
            "ALTER TABLE tasks ADD COLUMN review_required_detail TEXT",
        ),
        (
            "pr_state_polled_at",
            "ALTER TABLE tasks ADD COLUMN pr_state_polled_at TEXT",
        ),
        (
            "merge_queue_state",
            "ALTER TABLE tasks ADD COLUMN merge_queue_state TEXT",
        ),
    ] {
        if !table_has_column(conn, "tasks", column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

/// Add the external-tracker binding columns to `products` and the
/// per-work-item upstream-ref columns to `tasks`, plus the two partial
/// indices that support efficient lookup and uniqueness enforcement.
/// Idempotent — each column add is guarded by `table_has_column`, and
/// both indices use `CREATE … IF NOT EXISTS`.
///
/// Design: `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`
/// Schema section and R6.
fn migrate_external_tracker_columns(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "external_tracker_kind",
            "ALTER TABLE products ADD COLUMN external_tracker_kind TEXT",
        ),
        (
            "external_tracker_config",
            "ALTER TABLE products ADD COLUMN external_tracker_config TEXT",
        ),
    ] {
        if !table_has_column(conn, "products", column)? {
            conn.execute(ddl, [])?;
        }
    }
    for (column, ddl) in [
        (
            "external_ref_kind",
            "ALTER TABLE tasks ADD COLUMN external_ref_kind TEXT",
        ),
        (
            "external_ref_canonical_id",
            "ALTER TABLE tasks ADD COLUMN external_ref_canonical_id TEXT",
        ),
        (
            "external_ref_raw",
            "ALTER TABLE tasks ADD COLUMN external_ref_raw TEXT",
        ),
        (
            "external_ref_synced_at",
            "ALTER TABLE tasks ADD COLUMN external_ref_synced_at TEXT",
        ),
        (
            "external_ref_unbound_at",
            "ALTER TABLE tasks ADD COLUMN external_ref_unbound_at TEXT",
        ),
    ] {
        if !table_has_column(conn, "tasks", column)? {
            conn.execute(ddl, [])?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS tasks_external_ref_idx
             ON tasks (external_ref_kind, external_ref_canonical_id)
          WHERE external_ref_canonical_id IS NOT NULL;

         CREATE UNIQUE INDEX IF NOT EXISTS tasks_external_ref_bound_uniq
             ON tasks (external_ref_kind, external_ref_canonical_id)
          WHERE external_ref_canonical_id IS NOT NULL
            AND external_ref_unbound_at  IS NULL
            AND deleted_at               IS NULL;",
    )?;
    Ok(())
}

/// Create the `metrics_counter` / `metrics_gauge` tables for the
/// engine counter-metrics framework (phase 1). Idempotent — the
/// framework upserts on every flush, so re-running the migration is
/// a no-op on tables that already exist. Schemas match design
/// §"Persistence: state.db table".
fn migrate_metrics_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metrics_counter (
             name           TEXT PRIMARY KEY,
             value          INTEGER NOT NULL,
             updated_at_ms  INTEGER NOT NULL,
             description    TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS metrics_gauge (
             name             TEXT PRIMARY KEY,
             value            INTEGER NOT NULL,
             observed_at_ms   INTEGER NOT NULL,
             description      TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// One row pulled from `metrics_counter`. The framework rehydrates
/// these into the in-memory registry on engine start so monotonic
/// totals span restarts.
#[derive(Debug, Clone)]
pub struct MetricsCounterRow {
    pub name: String,
    pub value: u64,
    pub updated_at_ms: i64,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct MetricsGaugeRow {
    pub name: String,
    pub value: i64,
    pub observed_at_ms: i64,
    pub description: String,
}

impl WorkDb {
    /// Load every persisted counter and gauge row for the
    /// metrics-framework startup rehydrate. Order is unspecified
    /// (the caller is `metrics::seed_from_db`, which doesn't care).
    pub fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)> {
        let conn = self.connect()?;
        let mut counter_stmt = conn.prepare(
            "SELECT name, value, updated_at_ms, description FROM metrics_counter",
        )?;
        let counters: Vec<MetricsCounterRow> = counter_stmt
            .query_map([], |row| {
                let value_i64: i64 = row.get(1)?;
                Ok(MetricsCounterRow {
                    name: row.get(0)?,
                    // Counters round-trip as raw bits so monotonic
                    // u64 values above i64::MAX (theoretical only —
                    // see design §"Bounded memory and disk cost")
                    // survive the encode/decode.
                    value: value_i64 as u64,
                    updated_at_ms: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut gauge_stmt = conn.prepare(
            "SELECT name, value, observed_at_ms, description FROM metrics_gauge",
        )?;
        let gauges: Vec<MetricsGaugeRow> = gauge_stmt
            .query_map([], |row| {
                Ok(MetricsGaugeRow {
                    name: row.get(0)?,
                    value: row.get(1)?,
                    observed_at_ms: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((counters, gauges))
    }

    /// UPSERT every counter and gauge snapshot in a single
    /// transaction. The flush task calls this every 30s; the
    /// graceful-shutdown path calls it once more before the engine
    /// exits. Rehydrated "stale" rows (whose name no longer matches
    /// any registered handle) are skipped — the existing row stays
    /// in the table untouched so historical answers remain
    /// queryable (design §"Risks / open questions" item 3).
    pub fn metrics_flush(
        &self,
        counters: &[MetricsCounterRow],
        gauges: &[MetricsGaugeRow],
    ) -> Result<()> {
        if counters.is_empty() && gauges.is_empty() {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        for c in counters {
            tx.execute(
                "INSERT INTO metrics_counter (name, value, updated_at_ms, description)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                     value = excluded.value,
                     updated_at_ms = excluded.updated_at_ms,
                     description = excluded.description",
                params![c.name, c.value as i64, c.updated_at_ms, c.description],
            )?;
        }
        for g in gauges {
            tx.execute(
                "INSERT INTO metrics_gauge (name, value, observed_at_ms, description)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                     value = excluded.value,
                     observed_at_ms = excluded.observed_at_ms,
                     description = excluded.description",
                params![g.name, g.value, g.observed_at_ms, g.description],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Zero one metric (counter or gauge) in `state.db`. Called from
    /// the `MetricsReset` RPC handler after the in-memory atomic is
    /// already cleared. Returns `(counter_cleared, gauge_cleared)` so
    /// the caller can tell the operator which kind was found.
    pub fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)> {
        let conn = self.connect()?;
        let counter_rows = conn.execute(
            "UPDATE metrics_counter SET value = 0, updated_at_ms = ?2 WHERE name = ?1",
            params![name, now_ms],
        )?;
        let gauge_rows = conn.execute(
            "UPDATE metrics_gauge SET value = 0, observed_at_ms = ?2 WHERE name = ?1",
            params![name, now_ms],
        )?;
        Ok((counter_rows > 0, gauge_rows > 0))
    }

    /// Zero every counter and gauge row in `state.db`. Called from
    /// the `MetricsReset { name: None }` path (reset --all). Returns
    /// `(counters_cleared, gauges_cleared)`.
    pub fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let counter_rows = tx.execute(
            "UPDATE metrics_counter SET value = 0, updated_at_ms = ?1",
            params![now_ms],
        )?;
        let gauge_rows = tx.execute(
            "UPDATE metrics_gauge SET value = 0, observed_at_ms = ?1",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok((counter_rows, gauge_rows))
    }
}

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
fn allocate_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
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

/// Validate the `(execution_id, work_item_id)` discriminant on a
/// `CreateAttentionItemInput` and return the canonical pair to write.
/// Exactly one of the two must be set; both-set or neither-set is a
/// caller bug. Also confirms the referenced row actually exists so
/// the CHECK constraint and FK don't blow up on insert.
fn attention_target_from_input(
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
        (Some(_), Some(_)) => bail!(
            "attention item must reference either execution_id or work_item_id, not both"
        ),
        (None, None) => bail!(
            "attention item must reference either execution_id or work_item_id"
        ),
    }
}

/// Emit a sticky `repo_unresolved` attention item against
/// `work_item_id`, unless one is already open. Idempotent: repeated
/// reconcile passes against the same work item don't pile up rows.
/// Caller supplies the kind label (`task`, `chore`, `project`) so
/// the message names the right CLI verb.
fn record_repo_unresolved_attention(
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

/// The exact message text both the attention item and the
/// `request_execution` bail path use. Single source so the two
/// surfaces never drift, per the design doc's R1 mitigation.
fn repo_unresolved_attention_body(work_item_id: &str, kind_label: &str) -> String {
    format!(
        "work item {work_item_id} has no repo resolution; set one with `boss {kind_label} update --repo <url>` or set a product default."
    )
}

/// Kind label for the `boss <kind> update` hint in the
/// `repo_unresolved` message. Tasks under a project use `task`;
/// project-less rows are `chore`. Projects don't dispatch directly,
/// so the message there falls back to the safe generic.
fn repo_unresolved_kind_label(conn: &Connection, work_item_id: &str) -> Result<&'static str> {
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
        runtimes.push(query_task_runtime(conn, &task.id)?);
    }
    Ok(runtimes)
}

fn query_task_runtime(conn: &Connection, work_item_id: &str) -> Result<TaskRuntime> {
    let execution = query_latest_execution_for_work_item(conn, work_item_id)?;
    let (execution_status, run_status, execution_id, current_run_id) =
        if let Some(execution) = execution {
            let latest_run = query_latest_run(conn, &execution.id)?;
            let (run_status, run_id) = match latest_run {
                Some((id, status)) => (Some(status), Some(id)),
                None => (None, None),
            };
            (Some(execution.status), run_status, Some(execution.id), run_id)
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

fn query_latest_run(conn: &Connection, execution_id: &str) -> Result<Option<(String, String)>> {
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

fn query_latest_execution_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at,
                pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before
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
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
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

/// Resolve the canonical repo URL for a work item. Reads
/// `tasks.repo_remote_url` first — when set and non-empty, it wins as
/// the per-row override — and otherwise falls back to the parent
/// `products.repo_remote_url`. `None` for both → `Ok(None)` (the
/// caller decides what to do; today's dispatcher will record a
/// `repo_unresolved` attention item per multi-repo Q5).
///
/// No project layer: projects don't carry their own override (Q2),
/// they inherit transitively through their tasks. A non-task
/// `work_item_id` therefore returns `Ok(None)` since project / product
/// rows don't dispatch on their own.
///
/// Errors only when the task row references a `product_id` that is no
/// longer in the products table (an orphan task — a referential-
/// integrity break the caller should surface, not paper over with a
/// silent fallback).
///
/// This is the single resolution point per the multi-repo design's R1
/// mitigation: every dispatch and listing surface must route through
/// this helper so the rule never diverges.
pub(crate) fn resolve_repo_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT repo_remote_url, product_id FROM tasks WHERE id = ?1",
            [work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let Some((override_repo, product_id)) = row else {
        return Ok(None);
    };

    if let Some(url) = override_repo.as_deref().filter(|s| !s.is_empty()) {
        return Ok(Some(url.to_owned()));
    }

    let product = query_product(conn, &product_id)?.with_context(|| {
        format!("orphan task {work_item_id}: parent product {product_id} missing")
    })?;
    Ok(product.repo_remote_url)
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

    // Multi-repo Q5: route through the single resolver so per-row
    // overrides on `tasks.repo_remote_url` beat the product default.
    // Errors keep the same shape the bossctl path expects.
    resolve_repo_for_work_item(conn, work_item_id)?.with_context(|| {
        format!(
            "work item {work_item_id} does not resolve to a repo_remote_url; provide one explicitly"
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

/// Actor literal recorded against `project_property_audit` rows
/// produced by CLI / app callers (`SetProjectDesignDoc` RPC). Boss
/// is single-user today (per design Q10), so this is currently the
/// only "human" actor; the field exists so a future multi-user
/// layer can swap in caller identity without a schema change.
pub const AUDIT_ACTOR_HUMAN: &str = "human";

/// Actor literal recorded when the engine's design-doc detector
/// auto-populates an empty project pointer (sync rule 1 of design
/// Q6, via `sync_project_design_doc_from_detector`).
pub const AUDIT_ACTOR_DESIGN_DETECTOR: &str = "engine_design_detector";

/// A single append-only row in the `project_property_audit` table.
/// Records that `actor` changed `property` on `project_id` from
/// `old_value` to `new_value` at `changed_at` (epoch seconds, the
/// same format as `projects.updated_at`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPropertyAuditEntry {
    pub id: String,
    pub project_id: String,
    pub property: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: String,
    pub changed_at: String,
}

/// Emit one `project_property_audit` row for each of the three
/// `design_doc_*` columns whose value actually changed between
/// `before` and `after`. No-op when nothing changed (e.g. an
/// `unset = true` call on a project that was already unset, or a
/// branch-only edit that matched the existing branch). Runs inside
/// the caller's transaction so the audit row commits with the
/// underlying write.
fn record_design_doc_audit(
    conn: &Connection,
    project_id: &str,
    before: &Project,
    after: &Project,
    actor: &str,
    now: &str,
) -> Result<()> {
    let columns: [(&str, &Option<String>, &Option<String>); 3] = [
        (
            "design_doc_repo_remote_url",
            &before.design_doc_repo_remote_url,
            &after.design_doc_repo_remote_url,
        ),
        (
            "design_doc_branch",
            &before.design_doc_branch,
            &after.design_doc_branch,
        ),
        (
            "design_doc_path",
            &before.design_doc_path,
            &after.design_doc_path,
        ),
    ];
    for (property, old, new) in columns {
        if old == new {
            continue;
        }
        let id = next_id("paud");
        conn.execute(
            "INSERT INTO project_property_audit
                (id, project_id, property, old_value, new_value, actor, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, project_id, property, old, new, actor, now],
        )?;
    }
    Ok(())
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

/// Validate a caller-supplied `design_doc_path` per design Q8.
///
/// Rules: relative path (no leading `/`), no `..` segments, not
/// blank, must reference a markdown file (`.md` or `.markdown`).
/// Path is trimmed before storage so the column always reflects the
/// canonical form. Callers that want to *clear* the pointer should
/// use `unset = true` on `SetProjectDesignDocInput` instead of
/// passing an empty string here.
fn validate_design_doc_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("design_doc_path may not be empty (use `unset = true` to clear the pointer)");
    }
    if trimmed.starts_with('/') {
        bail!("design_doc_path must be repo-relative (no leading `/`): {trimmed}");
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        bail!("design_doc_path may not contain `..` segments: {trimmed}");
    }
    // Cube workspace paths are ephemeral machine-local locations that
    // become invalid once the workspace is re-leased to a different task.
    // They must never be persisted as a design-doc pointer; GitHub is the
    // durable store. Reject any path that looks like a workspace-relative
    // path escaped into the repo-relative field.
    if trimmed.contains("cube/workspaces/") {
        bail!(
            "design_doc_path must not reference a cube workspace path \
             (contains 'cube/workspaces/'): {trimmed}"
        );
    }
    if !(trimmed.ends_with(".md") || trimmed.ends_with(".markdown")) {
        bail!("design_doc_path must reference a markdown file (.md or .markdown): {trimmed}");
    }
    Ok(trimmed.to_owned())
}

/// Canonicalise a caller-supplied repo remote URL into the same shape
/// stored on `products.repo_remote_url`. Shared between every column
/// that holds a repo URL: product default, task / chore override,
/// project design-doc pointer. Today the canonical form is just
/// `trim + blank→None`; lift to a richer `(scheme, owner, repo, .git)`
/// canonicaliser here when the column grows one — every write site
/// already routes through this function.
pub fn canonicalize_repo_remote_url(value: Option<String>) -> Option<String> {
    normalize_optional_text(value)
}

/// Enforce the repo-override invariant for task / chore inserts.
///
/// Rule: a task row carries `repo_remote_url` only when its parent
/// product has **no** repo of its own (multi-repo products). When the
/// product has a repo, the row must be `NULL`; the resolved repo is
/// always the product's.
///
/// Returns the canonicalised URL to write, or `None` when the product
/// owns the repo. Errors when the caller violates the invariant:
///   - product has a repo AND caller supplied a non-empty override
///   - product has no repo AND caller supplied no repo
fn enforce_task_repo_invariant(
    product: &Product,
    input_repo: Option<String>,
) -> Result<Option<String>> {
    let canonicalized = canonicalize_repo_remote_url(input_repo);
    if let Some(product_repo) = product.repo_remote_url.as_deref() {
        if canonicalized.is_some() {
            bail!(
                "cannot set per-task repo override on product `{}`: \
                 product has its own repo (`{}`). \
                 Clear the product's repo first, or omit --repo to inherit.",
                product.slug,
                product_repo,
            );
        }
        Ok(None)
    } else {
        match canonicalized {
            Some(url) => Ok(Some(url)),
            None => bail!(
                "work item under product `{}` has no repo; \
                 provide one via repo_remote_url (product has no default).",
                product.slug,
            ),
        }
    }
}

/// Per design Q3 (`tools/boss/docs/designs/multi-repo-work-modeling.md`):
/// strip protocol + host, take the path basename minus `.git`.
///   `git@github.com:foo/bar.git` → `bar`
///   `https://github.com/foo/bar.git` → `bar`
/// Pure-string parse — no registry. Used by the CLI to match
/// `--repo <selector>` against a resolved repo URL.
pub fn short_name_for(url: &str) -> &str {
    let after_slash = url.rsplit('/').next().unwrap_or(url);
    let after_colon = after_slash.rsplit(':').next().unwrap_or(after_slash);
    after_colon.trim_end_matches(".git")
}

/// Thin wrapper kept for the design-doc call sites until they migrate
/// to [`canonicalize_repo_remote_url`] directly.
fn canonicalize_design_doc_repo_remote_url(value: Option<String>) -> Option<String> {
    canonicalize_repo_remote_url(value)
}

/// Build the GitHub web URL for a design doc per the design's Q5
/// recipe (`https://github.com/<owner>/<repo>/blob/<branch>/<path>`).
/// Falls back to a best-effort blob URL when the repo doesn't parse
/// as a `github.com` URL (e.g. an enterprise mirror) so the caller
/// always gets *something* to render — the resolver itself doesn't
/// fail the whole request just because the URL formatter can't pull
/// `owner/repo` out of the remote.
fn render_design_doc_web_url(repo_remote_url: &str, branch: &str, path: &str) -> String {
    match crate::completion::parse_repo_slug(repo_remote_url) {
        Ok(slug) => format!("https://github.com/{slug}/blob/{branch}/{path}"),
        Err(_) => format!("{repo_remote_url}/blob/{branch}/{path}"),
    }
}

/// Build the GitHub raw-content URL for a design doc:
/// `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/<path>`.
/// Returns `None` when the repo URL can't be parsed as a github.com URL
/// (e.g. an enterprise mirror or non-GitHub host) so callers know the
/// raw-content fast path is unavailable and should fall back to the
/// web URL.
fn render_design_doc_raw_content_url(
    repo_remote_url: &str,
    branch: &str,
    path: &str,
) -> Option<String> {
    crate::completion::parse_repo_slug(repo_remote_url)
        .ok()
        .map(|slug| format!("https://raw.githubusercontent.com/{slug}/{branch}/{path}"))
}

/// Look up a product by `repo_remote_url`. Used by
/// `resolve_project_design_doc` to classify a resolved repo as
/// `OtherProduct` (Boss tracks it) vs `External` (we don't). Returns
/// `None` when no product matches. `NULL` `repo_remote_url` rows are
/// excluded so a freshly-created product without a URL doesn't
/// silently match the project's pointer.
fn find_product_by_repo_remote_url(conn: &Connection, repo_remote_url: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM products
         WHERE repo_remote_url IS NOT NULL AND repo_remote_url = ?1
         ORDER BY created_at ASC, id ASC
         LIMIT 1",
        [repo_remote_url],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

fn apply_text_patch(target: &mut String, patch: Option<String>) {
    if let Some(value) = patch {
        *target = value;
    }
}

/// Apply a `WorkItemPatch.repo_remote_url` update with the canonical
/// "empty-string clears" wire convention. `None` patch means "leave
/// the column alone." `Some("")` (or any whitespace-only string)
/// means "clear the override / inherit." Otherwise canonicalise and
/// store the value. Shared between product / task / chore update
/// paths so a single rule governs every repo URL column.
fn apply_repo_remote_url_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = canonicalize_repo_remote_url(Some(value));
    }
}

fn apply_optional_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = normalize_optional_text(Some(value));
    }
}

/// `WorkItemPatch.model_override` / `WorkItemPatch.default_model`
/// share the "empty string clears, otherwise store verbatim" wire
/// shape: `None` leaves the column alone, `Some("")` writes NULL,
/// and `Some(slug)` stores the slug after a trim. Slugs are
/// deliberately not validated — claude is the source of truth on
/// what `--model` accepts (design §Q3).
fn apply_optional_string_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        let trimmed = value.trim();
        *target = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
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

/// If `id` looks like a friendly work-item selector (`T42`, `t42`, `P7`,
/// `p7`), query the DB by short_id and return the matching primary id.
/// Returns `Ok(None)` when `id` is not a friendly-id form or when no row
/// matches; callers should treat the original id as-is in that case.
fn resolve_friendly_work_item_id(conn: &Connection, id: &str) -> Result<Option<String>> {
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
    if let Some(primary_id) = conn
        .query_row(
            "SELECT id FROM tasks WHERE short_id = ?1 AND deleted_at IS NULL LIMIT 1",
            params![n],
            |row| row.get::<_, String>(0),
        )
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
/// Used at edge-creation time (`add_dependency`): a brand-new edge
/// that introduces a gating prereq must move its dependent to
/// `blocked` so the kanban and dispatcher reflect the new gate.
/// The reverse (cascade-on-prereq-regression) deliberately does NOT
/// call this — see the comment on
/// [`cascade_dependents_after_prereq_status_change`].
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
fn maybe_engine_unblock_dependent(
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
fn cascade_dependents_after_prereq_status_change(
    conn: &Connection,
    prereq_id: &str,
    new_prereq_status: &str,
    now_epoch: &str,
) -> Result<()> {
    if !deps::status_satisfies(prereq_id, new_prereq_status) {
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

fn lookup_blocked_reason(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
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

    /// Returns the `:memory:` sentinel so `WorkDb::open` allocates a
    /// per-test named shared-cache in-memory database. Each call to
    /// `WorkDb::open(PathBuf::from(":memory:"))` gets a unique database;
    /// the `label` parameter is kept for call-site readability only.
    fn temp_db_path(_label: &str) -> PathBuf {
        PathBuf::from(":memory:")
    }

    /// Returns a real on-disk temp path. Use this only for tests that
    /// open a raw `rusqlite::Connection` alongside the `WorkDb` (e.g.
    /// schema-migration tests that pre-populate a legacy schema and then
    /// re-open via `WorkDb::open`). All other tests should use
    /// `temp_db_path` so the database stays in RAM.
    fn disk_db_path(label: &str) -> PathBuf {
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
    fn metadata_get_returns_none_for_missing_key() {
        let path = temp_db_path("meta-missing");
        let db = WorkDb::open(path).unwrap();
        let value = db.get_metadata("never_written").unwrap();
        assert!(value.is_none());
    }

    #[test]
    fn metadata_set_then_get_round_trips() {
        let path = temp_db_path("meta-roundtrip");
        let db = WorkDb::open(path).unwrap();
        db.set_metadata("live_status_disabled_slots", "1,3,7")
            .unwrap();
        let value = db.get_metadata("live_status_disabled_slots").unwrap();
        assert_eq!(value.as_deref(), Some("1,3,7"));
    }

    #[test]
    fn metadata_set_replaces_prior_value_for_same_key() {
        let path = temp_db_path("meta-replace");
        let db = WorkDb::open(path).unwrap();
        db.set_metadata("live_status_disabled_slots", "1,3")
            .unwrap();
        db.set_metadata("live_status_disabled_slots", "5,7")
            .unwrap();
        let value = db.get_metadata("live_status_disabled_slots").unwrap();
        assert_eq!(value.as_deref(), Some("5,7"));
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
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Plan".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Plan".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            },
            CreateTaskInput {
                product_id: product.id.clone(),
                project_id: "proj_does_not_exist".to_owned(),
                name: "Bad".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();

        let inputs = (0..3)
            .map(|i| CreateChoreInput {
                product_id: product.id.clone(),
                name: format!("Chore {i}"),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let chore_running = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Running".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
        assert!(
            runtime_running.current_run_id.is_some(),
            "running chore must surface its work_runs id so chore show resolves \
             current_run_id without going through events.sock",
        );
        assert!(runtime_idle.current_run_id.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// `get_task_runtime` returns the same per-task runtime shape that
    /// `WorkTree::task_runtimes` carries, sourced from the engine's
    /// own execution/run tables. The data path must populate
    /// `execution_id` from the moment `request_execution` accepts the
    /// dispatch (status=`ready`, no run yet), and add
    /// `current_run_id` once `start_execution_run` commits — without
    /// waiting for any hook event. This is the contract `bossctl
    /// agents list` and `boss chore show` rely on so the visibility
    /// surface stops lagging behind events.sock.
    #[test]
    fn get_task_runtime_tracks_execution_then_run_id() {
        let path = temp_db_path("runtime_single");
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
                name: "Investigate".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        // Pre-dispatch: nothing in flight, every field is `None`.
        let pre = db.get_task_runtime(&chore.id).unwrap();
        assert_eq!(pre.work_item_id, chore.id);
        assert!(pre.execution_status.is_none());
        assert!(pre.execution_id.is_none());
        assert!(pre.current_run_id.is_none());

        // After dispatch acceptance (request_execution via reconcile):
        // execution_id is populated, run_id is still None because no
        // work_runs row has been created yet.
        db.reconcile_product_executions(&product.id).unwrap();
        let after_ready = db.get_task_runtime(&chore.id).unwrap();
        assert_eq!(after_ready.execution_status.as_deref(), Some("ready"));
        assert!(after_ready.execution_id.is_some());
        assert!(
            after_ready.current_run_id.is_none(),
            "ready execution has no work_runs row yet",
        );

        // After start_execution_run: a work_runs row exists; both
        // execution_id and current_run_id are populated.
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();
        let after_run = db.get_task_runtime(&chore.id).unwrap();
        assert_eq!(after_run.execution_status.as_deref(), Some("running"));
        assert_eq!(after_run.execution_id.as_deref(), Some(execution.id.as_str()));
        assert_eq!(after_run.current_run_id.as_deref(), Some(run.id.as_str()));
        assert_eq!(after_run.run_status.as_deref(), Some("active"));

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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Dependent".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let other_dependent = db
            .create_chore(CreateChoreInput {
                product_id: other_product.id.clone(),
                name: "Other Dependent".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
        let path = disk_db_path("pre-v3");
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
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                execution_id: Some(execution.id.clone()),
                work_item_id: None,
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
                .contains("does not resolve to a repo_remote_url"),
            "expected resolver error in `{err}`",
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
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                no_design_task: false,
            })
            .unwrap();
        // Product has no repo and task has no override — enforce_task_repo_invariant
        // now blocks this combination at the API layer. Insert via raw SQL to
        // represent a pre-existing legacy row that the reconciler must handle.
        let task_id = {
            let conn = db.connect().unwrap();
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, ?3, 'project_task', 'First', '', 'todo', 1, NULL, NULL, ?4, ?4, 1, 'medium', 'test')",
                params![id, product.id, project.id, now],
            ).unwrap();
            id
        };

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
                effort_level: None,
                model_override: None,
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        // Now there's exactly one executable item under the project
        // (the user task; the design is `done`), so reconcile creates
        // exactly one execution.
        assert_eq!(second_pass.created.len(), 1);

        let task_execution = db.list_executions(Some(&task_id)).unwrap();
        assert_eq!(task_execution.len(), 1);
        assert_eq!(task_execution[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    /// Acceptance (a) for multi-repo Q5 dispatch wiring: a chore that
    /// supplies its own `repo_remote_url` override dispatches against
    /// the override URL, not the parent product default. The two URLs
    /// differ so the test can't accidentally pass on inheritance.
    #[test]
    fn reconcile_dispatches_chore_against_repo_override() {
        let path = temp_db_path("reconcile-chore-override");
        let db = WorkDb::open(path.clone()).unwrap();

        // Product has no default repo — the chore provides its own override.
        // (enforce_task_repo_invariant rejects setting both simultaneously.)
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
                name: "Nimbus migration".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: Some("git@github.com:myorg/nimbus.git".to_owned()),
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(result.created.len(), 1, "chore should dispatch on first pass");
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(
            executions[0].repo_remote_url,
            "git@github.com:myorg/nimbus.git",
            "the row should carry the chore's override, not the product default",
        );
        assert_eq!(executions[0].status, "ready");

        // No sticky attention items raised for a resolvable row.
        assert!(
            db.list_attention_items_for_work_item(&chore.id)
                .unwrap()
                .is_empty(),
        );

        let _ = std::fs::remove_file(path);
    }

    /// Acceptance (b) for multi-repo Q5 dispatch wiring: a chore with
    /// no resolvable repo (product has no default and the chore has
    /// no override) surfaces a sticky `repo_unresolved`
    /// `WorkAttentionItem` and creates NO execution row. The item
    /// dedupes across reconcile passes so the user doesn't end up with
    /// a pile of identical entries.
    #[test]
    fn reconcile_surfaces_repo_unresolved_attention_when_unresolvable() {
        let path = temp_db_path("reconcile-repo-unresolved");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        // Neither product nor chore has a repo — enforce_task_repo_invariant now
        // blocks this at the API layer. Insert via raw SQL to represent a
        // pre-existing legacy row that the reconciler must handle gracefully.
        let chore_id = {
            let conn = db.connect().unwrap();
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, NULL, 'chore', 'Orphan', '', 'todo', NULL, NULL, NULL, ?3, ?3, 1, 'medium', 'test')",
                params![id, product.id, now],
            ).unwrap();
            id
        };

        let first_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(first_pass.created.is_empty(), "no execution row when repo unresolved");
        assert!(
            db.list_executions(None).unwrap().is_empty(),
            "the failure is sticky-via-attention, not via a phantom execution row",
        );

        let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
        assert_eq!(items.len(), 1, "one sticky attention item per work item");
        let item = &items[0];
        assert_eq!(item.kind, "repo_unresolved");
        assert_eq!(item.status, "open");
        assert_eq!(item.execution_id, None);
        assert_eq!(item.work_item_id.as_deref(), Some(chore_id.as_str()));
        assert!(
            item.body_markdown.contains("boss chore update --repo <url>"),
            "body should tell the user how to fix it (got `{}`)",
            item.body_markdown,
        );
        assert!(
            item.body_markdown.contains(&chore_id),
            "body should name the work item (got `{}`)",
            item.body_markdown,
        );

        // Sticky: a second reconcile pass does not duplicate the row.
        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(second_pass.created.is_empty());
        assert!(second_pass.updated.is_empty());
        assert_eq!(
            db.list_attention_items_for_work_item(&chore_id)
                .unwrap()
                .len(),
            1,
        );

        let _ = std::fs::remove_file(path);
    }

    /// Acceptance (c) for multi-repo Q5 dispatch wiring: the explicit
    /// `bossctl work start` path (i.e.
    /// `request_execution_with_live_check`) refuses with the same
    /// error message the reconciler's attention item carries, and it
    /// raises (or reuses) the sticky attention item so the kanban
    /// also surfaces the failure.
    #[test]
    fn request_execution_refuses_when_repo_unresolvable() {
        let path = temp_db_path("request-exec-repo-unresolved");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        // Neither product nor chore has a repo — enforce_task_repo_invariant now
        // blocks this at the API layer. Insert via raw SQL to represent a
        // pre-existing legacy row.
        let chore_id = {
            let conn = db.connect().unwrap();
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, NULL, 'chore', 'Orphan', '', 'todo', NULL, NULL, NULL, ?3, ?3, 1, 'medium', 'test')",
                params![id, product.id, now],
            ).unwrap();
            id
        };

        let err = db
            .request_execution_with_live_check(
                RequestExecutionInput {
                    work_item_id: chore_id.clone(),
                    priority: None,
                    preferred_workspace_id: None,
                    force: false,
                },
                |_| true,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("has no repo resolution"),
            "expected the same repo_unresolved error as reconcile (got `{err}`)",
        );
        assert!(
            err.to_string().contains("boss chore update --repo <url>"),
            "error should name the CLI fix (got `{err}`)",
        );

        assert!(
            db.list_executions(None).unwrap().is_empty(),
            "the refused request must not leave an execution row behind",
        );

        let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
        assert_eq!(items.len(), 1, "the kanban surface mirrors the CLI error");
        assert_eq!(items[0].kind, "repo_unresolved");

        let _ = std::fs::remove_file(path);
    }

    /// Adjunct to (a) above: once the user supplies an override URL
    /// on a previously-unresolvable chore, the next reconcile creates
    /// the execution row against that URL. The sticky attention item
    /// stays around until the user explicitly resolves it — the engine
    /// does not auto-resolve, matching the design's "sticky" wording.
    #[test]
    fn reconcile_dispatches_after_override_repairs_unresolvable_chore() {
        let path = temp_db_path("reconcile-repair-override");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        // Neither product nor chore has a repo — enforce_task_repo_invariant now
        // blocks this at the API layer. Insert via raw SQL to represent a
        // pre-existing legacy row.
        let chore_id = {
            let conn = db.connect().unwrap();
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, NULL, 'chore', 'Orphan', '', 'todo', NULL, NULL, NULL, ?3, ?3, 1, 'medium', 'test')",
                params![id, product.id, now],
            ).unwrap();
            id
        };

        let first_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(first_pass.created.is_empty());
        assert_eq!(
            db.list_attention_items_for_work_item(&chore_id)
                .unwrap()
                .len(),
            1,
        );

        db.update_work_item(
            &chore_id,
            WorkItemPatch {
                repo_remote_url: Some("git@github.com:myorg/nimbus.git".to_owned()),
                effort_level: None,
                model_override: None,
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(second_pass.created.len(), 1);
        let executions = db.list_executions(Some(&chore_id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(
            executions[0].repo_remote_url,
            "git@github.com:myorg/nimbus.git",
        );

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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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

        // The fresh run row starts with `transcript_path = NULL` (the
        // engine has no way to learn the path until the first hook
        // event arrives). The first time we see a transcript_path on
        // a hook payload, the dispatcher persists it here; subsequent
        // events must NOT overwrite it (a `/resume` session would
        // otherwise yank the path out from under the summarizer's
        // open file handle).
        //
        // Note the call passes `execution.id` (an `exec_*`), not
        // `run.id` (a `run_*`) — that mirrors what the dispatcher
        // actually feeds the function in production via the
        // `_boss_run_id` hook-payload field. The function resolves
        // to the latest run for the execution under the hood.
        assert!(reread.transcript_path.is_none());
        let first = db
            .set_run_transcript_path_if_unset(
                &execution.id,
                "/home/u/.claude/projects/foo/sess-1.jsonl",
            )
            .unwrap();
        assert_eq!(
            first,
            SetRunTranscriptPathOutcome::Updated,
            "first write must report Updated",
        );
        let after_first = db.get_run(&run.id).unwrap();
        assert_eq!(
            after_first.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        );
        let second = db
            .set_run_transcript_path_if_unset(
                &execution.id,
                "/home/u/.claude/projects/foo/sess-2.jsonl",
            )
            .unwrap();
        assert_eq!(
            second,
            SetRunTranscriptPathOutcome::AlreadySet,
            "second write must be a no-op",
        );
        let after_second = db.get_run(&run.id).unwrap();
        assert_eq!(
            after_second.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
            "first writer must win — never clobber the path the summarizer tail watcher already opened",
        );

        // Read-side companion: `transcript_path_for_execution` must
        // resolve the same execution id to the path the write side
        // just stored, AND must do so when handed an `exec_*` (the
        // namespace the in-engine read sites all carry). Pre-fix the
        // engine used `get_run(execution_id)` instead and silently
        // got `Err(unknown run)`, which is what kept the slot
        // snapshot's `transcript_path` pinned at NULL even after the
        // write path was fixed in PR #384.
        assert_eq!(
            db.transcript_path_for_execution(&execution.id)
                .unwrap()
                .as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
            "read-side lookup must accept an execution id and return the latest run's transcript_path",
        );
        assert!(
            db.transcript_path_for_execution(&run.id)
                .unwrap()
                .is_none(),
            "read-side lookup with a work_runs.id (wrong namespace) must NOT silently return a sibling run's path",
        );
        assert!(
            db.transcript_path_for_execution("exec-does-not-exist")
                .unwrap()
                .is_none(),
            "unknown execution must yield Ok(None), not Err — callers log-and-default",
        );

        // Unknown execution id must surface as RowMissing, not as a
        // silent no-op — that distinction is the whole point of the
        // 2026-05-12 chore, because conflating the two is what hid
        // the wrong-namespace bug behind a steady `_persist_noop=N`.
        let missing = db
            .set_run_transcript_path_if_unset("exec-does-not-exist", "/x.jsonl")
            .unwrap();
        assert_eq!(missing, SetRunTranscriptPathOutcome::RowMissing);

        // The same call passing a `work_runs.id` (the old, buggy
        // shape) must also surface as RowMissing — `run.id` is in a
        // different namespace from `execution_id` and must not match.
        // This pins the regression: pre-fix, passing `run.id` was
        // the production code path and would silently return false.
        let wrong_namespace = db
            .set_run_transcript_path_if_unset(&run.id, "/y.jsonl")
            .unwrap();
        assert_eq!(
            wrong_namespace,
            SetRunTranscriptPathOutcome::RowMissing,
            "passing a work_runs.id where an execution_id is expected must surface as RowMissing — not be silently absorbed as an AlreadySet no-op",
        );

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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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

    /// Reaping a running execution stamps it `orphaned` (terminal),
    /// preserves the cube workspace columns, and stamps any active
    /// work_runs with status='orphaned' + the supplied reason as the
    /// run's `result_summary`. The orphan path is what
    /// `bossctl agents reap` and the engine-startup reaper both call.
    #[test]
    fn mark_execution_orphaned_preserves_workspace_and_stamps_run() {
        let path = temp_db_path("reap-orphan");
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
                name: "Orphan candidate".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        let (_running, run) = db
            .start_execution_run(
                &execution.id,
                "worker-orphan",
                "mono",
                "lease-ORPH",
                "mono-agent-004",
                "/tmp/mono-agent-004",
            )
            .unwrap();

        let reason = "test reap: simulated pane death";
        let orphaned = db.mark_execution_orphaned(&execution.id, reason).unwrap();
        assert_eq!(orphaned.status, "orphaned");
        assert!(
            orphaned.finished_at.is_some(),
            "orphan reap must stamp finished_at",
        );
        // Workspace columns MUST be preserved — that's the whole
        // contract that lets the next worker resume the same branch.
        assert_eq!(orphaned.cube_lease_id.as_deref(), Some("lease-ORPH"));
        assert_eq!(orphaned.cube_workspace_id.as_deref(), Some("mono-agent-004"));
        assert_eq!(orphaned.workspace_path.as_deref(), Some("/tmp/mono-agent-004"));

        // And the run record gets the reason as its result_summary so
        // an operator can later see why the row went terminal.
        let run_after = db.get_run(&run.id).unwrap();
        assert_eq!(run_after.status, "orphaned");
        assert!(run_after.finished_at.is_some());
        assert_eq!(run_after.result_summary.as_deref(), Some(reason));

        let _ = std::fs::remove_file(path);
    }

    /// Reaping a row that's already terminal must error rather than
    /// silently no-op — same contract as `cancel_execution`. This is
    /// what stops the engine-startup reaper from racing the existing
    /// `heal_ghost_active_chores` sweep into a double-stamp.
    #[test]
    fn mark_execution_orphaned_errors_on_already_terminal() {
        let path = temp_db_path("reap-already-terminal");
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("completed".to_owned()),
                ..Default::default()
            })
            .unwrap();

        let err = db
            .mark_execution_orphaned(&execution.id, "test")
            .expect_err("terminal rows must refuse reap");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("terminal"),
            "expected terminal-status error, got: {msg}",
        );

        let _ = std::fs::remove_file(path);
    }

    /// After the startup reaper marks an execution `orphaned`, the
    /// next `reconcile_active_dispatch` pass creates a fresh `ready`
    /// row whose `preferred_workspace_id` inherits the orphan's
    /// `cube_workspace_id`. Without this, cube would lease any free
    /// workspace and the new worker would start against `main` on an
    /// unrelated branch — losing the in-progress work the orphan was
    /// driving.
    #[test]
    fn reconcile_inherits_workspace_id_from_orphaned_predecessor() {
        let path = temp_db_path("reconcile-orphan-workspace");
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
                name: "Resumable orphan".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // Drive the chore into `active` so reconcile considers it.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-orphan",
            "mono",
            "lease-ORPH",
            "mono-agent-005",
            "/tmp/mono-agent-005",
        )
        .unwrap();

        // Reaper pass: simulate the startup probe's Dead verdict.
        db.mark_execution_orphaned(&execution.id, "test orphan").unwrap();

        // Reconcile pass: predecessor is terminal-orphaned, so a fresh
        // ready row is inserted. The new row must inherit the orphan's
        // workspace_id as its preferred_workspace_id.
        let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);

        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 2);
        let orphan_after = executions.iter().find(|e| e.id == execution.id).unwrap();
        assert_eq!(orphan_after.status, "orphaned");
        // Workspace preserved on the orphan row.
        assert_eq!(orphan_after.cube_workspace_id.as_deref(), Some("mono-agent-005"));

        let fresh = executions.iter().find(|e| e.id != execution.id).unwrap();
        assert_eq!(fresh.status, "ready");
        assert_eq!(
            fresh.preferred_workspace_id.as_deref(),
            Some("mono-agent-005"),
            "fresh ready row must inherit the orphan's workspace_id so cube re-leases the same branch",
        );
        // The new row doesn't carry the cube lease directly — that's
        // set by `start_execution_run` when the dispatcher claims it.
        assert!(fresh.cube_lease_id.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// `reconcile_active_dispatch` does NOT inherit workspace_id from
    /// predecessors that landed in a different terminal status
    /// (`abandoned`, `cancelled`, `failed`). Those are intentional
    /// throwaways — a redispatch shouldn't drag forward a workspace
    /// the human or engine deliberately cut loose.
    #[test]
    fn reconcile_does_not_inherit_workspace_from_non_orphaned_terminal() {
        let path = temp_db_path("reconcile-no-inherit");
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
                name: "Cancelled predecessor".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-cancel",
            "mono",
            "lease-CAN",
            "mono-agent-006",
            "/tmp/mono-agent-006",
        )
        .unwrap();
        db.cancel_execution(&execution.id).unwrap();
        // Cancel reset kanban → todo. Drive it back to active so
        // reconcile re-considers the work item.
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

        let executions = db.list_executions(Some(&chore.id)).unwrap();
        let fresh = executions.iter().find(|e| e.id != execution.id).unwrap();
        assert!(
            fresh.preferred_workspace_id.is_none(),
            "cancelled predecessor must NOT propagate workspace_id forward; got {fresh:?}",
        );

        let _ = std::fs::remove_file(path);
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let done_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already done".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                no_design_task: false,
            })
            .unwrap();

        // The project comes with a `kind = 'design'` task already
        // attached, named "Design <project name>" and parked at `ordinal = 0` so it
        // sorts first in the project's chain.
        let tasks = db.list_tasks(&product.id, Some(&project.id), None).unwrap();
        let design = tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("project should have an auto-created design task");
        assert_eq!(design.name, "Design Engine dispatch instrumentation");
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
                no_design_task: false,
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

    /// `--no-design-task` creates the project with zero child tasks —
    /// the seed task is not inserted at all. Reconcile should find
    /// nothing to dispatch.
    #[test]
    fn create_project_no_design_task_creates_project_alone() {
        let path = temp_db_path("project-no-design-task");
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
                name: "Incident 001 postmortem".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: true,
            })
            .unwrap();

        let tasks = db.list_tasks(&product.id, Some(&project.id), None).unwrap();
        assert!(
            tasks.is_empty(),
            "no_design_task project must have zero child tasks, found: {tasks:?}",
        );

        db.reconcile_product_executions(&product.id).unwrap();
        let executions = db.list_executions(Some(&project.id)).unwrap();
        assert!(
            executions.is_empty(),
            "no_design_task project must spawn no executions, found: {executions:?}",
        );

        let _ = std::fs::remove_file(path);
    }

    /// Pre-design-card databases don't have a design task per
    /// project. The migration fills the gap so the kanban renders
    /// existing projects the same way as new ones — a "Design"
    /// card sits at the head of each project's chain on next open.
    #[test]
    fn migration_backfills_design_tasks_for_existing_projects() {
        let path = disk_db_path("migration-design-backfill");
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
                    no_design_task: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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

    /// `start_execution_run` clears `autostart` to `0` in the same
    /// transaction that advances `tasks.status` to `'active'`. After
    /// that single-shot consumption the reconciler must not re-dispatch
    /// the task if it is reset to `todo` (the Done→Backlog gesture).
    #[test]
    fn start_execution_run_clears_autostart() {
        let path = temp_db_path("start-run-clears-autostart");
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
                name: "Autostart chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert!(chore.autostart, "newly created chore should have autostart=true");

        // Place a ready execution so start_execution_run can run.
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
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

        match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert_eq!(t.status, "active");
                assert!(!t.autostart, "autostart must be cleared after first Doing transition");
            }
            other => panic!("expected chore/task, got {other:?}"),
        }

        // Simulate Done→Backlog: move back to todo. With autostart
        // consumed, reconcile_product_executions must NOT create a
        // new ready execution (task_accepts_execution returns false
        // when autostart=false and status='todo').
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("todo".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let executions_before = db.list_executions(Some(&chore.id)).unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        let executions_after = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(
            executions_before.len(),
            executions_after.len(),
            "reset-to-todo task with consumed autostart must not be re-dispatched by reconcile",
        );
    }

    /// The backfill migration clears `autostart` for rows that are
    /// already past their first Doing transition (status != 'todo') so
    /// single-shot semantics apply to pre-migration databases.
    #[test]
    fn migrate_backfill_autostart_consumed_clears_non_todo_rows() {
        let path = disk_db_path("autostart-backfill");
        // Pre-populate a v9 schema with rows in various statuses, all
        // with autostart = 1, then re-open via WorkDb to run migrations.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // Minimal schema that satisfies the migration chain up to v9.
            conn.execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE products (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                     description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
                     status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                     auto_pr_maintenance_enabled INTEGER NOT NULL DEFAULT 0,
                     default_model TEXT, dispatch_preamble TEXT,
                     ci_attempt_budget INTEGER);
                 CREATE TABLE projects (
                     id TEXT PRIMARY KEY, product_id TEXT NOT NULL, name TEXT NOT NULL,
                     slug TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                     goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                     priority TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                     last_status_actor TEXT NOT NULL DEFAULT 'human',
                     design_doc TEXT, design_doc_updated_at TEXT, design_doc_draft TEXT,
                     short_id INTEGER);
                 CREATE TABLE tasks (
                     id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                     kind TEXT NOT NULL DEFAULT 'chore', name TEXT NOT NULL,
                     description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                     ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                     created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                     autostart INTEGER NOT NULL DEFAULT 1,
                     last_status_actor TEXT NOT NULL DEFAULT 'human',
                     priority TEXT NOT NULL DEFAULT 'medium',
                     created_via TEXT NOT NULL DEFAULT 'human',
                     blocked_reason TEXT, blocked_attempt_id TEXT,
                     repo_remote_url TEXT,
                     effort_level TEXT, model_override TEXT,
                     ci_attempt_budget INTEGER, ci_attempts_used INTEGER NOT NULL DEFAULT 0,
                     short_id INTEGER);
                 CREATE TABLE IF NOT EXISTS short_id_sequences (
                     product_id TEXT PRIMARY KEY, next_value INTEGER NOT NULL DEFAULT 1);
                 INSERT INTO metadata(key, value) VALUES ('schema_version', '9');
                 INSERT INTO products VALUES (
                     'prod-1', 'Boss', 'boss', '', NULL, 'active',
                     '1700000000', '1700000000', 0, NULL, NULL, NULL);
                 INSERT INTO tasks VALUES (
                     'task-todo', 'prod-1', NULL, 'chore', 'Todo task', '', 'todo',
                     1, NULL, NULL, '1700000001', '1700000001', 1,
                     'human', 'medium', 'human', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL);
                 INSERT INTO tasks VALUES (
                     'task-active', 'prod-1', NULL, 'chore', 'Active task', '', 'active',
                     2, NULL, NULL, '1700000002', '1700000002', 1,
                     'human', 'medium', 'human', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL);
                 INSERT INTO tasks VALUES (
                     'task-done', 'prod-1', NULL, 'chore', 'Done task', '', 'done',
                     3, NULL, NULL, '1700000003', '1700000003', 1,
                     'human', 'medium', 'human', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL);
                 INSERT INTO tasks VALUES (
                     'task-blocked', 'prod-1', NULL, 'chore', 'Blocked task', '', 'blocked',
                     4, NULL, NULL, '1700000004', '1700000004', 1,
                     'human', 'medium', 'human', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL);",
            )
            .unwrap();
        }

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        let autostart_for = |id: &str| -> i64 {
            conn.query_row(
                "SELECT autostart FROM tasks WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };

        assert_eq!(autostart_for("task-todo"), 1, "todo row must keep autostart=1");
        assert_eq!(autostart_for("task-active"), 0, "active row must be cleared to autostart=0");
        assert_eq!(autostart_for("task-done"), 0, "done row must be cleared to autostart=0");
        assert_eq!(autostart_for("task-blocked"), 0, "blocked row must be cleared to autostart=0");

        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");

        let _ = std::fs::remove_file(path);
    }

    /// FIFO ordering: the active chore that was moved to `active`
    /// first should be the first one redispatched. Later kanban
    /// drags wait their turn.
    #[test]
    fn rescan_orders_candidates_by_updated_at_ascending() {
        let path = disk_db_path("rescan-fifo");
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
                    created_via: None,
                    repo_remote_url: None,
                    effort_level: None,
                    model_override: None,
                    force_duplicate: false,
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
        let path = disk_db_path("rescan-gated");
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Dependent".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
        // disk_db_path required: re-opens the DB to trigger migration.
        let path = disk_db_path("ts-migrate");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "ISO chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: p2.id,
                name: "Beta task".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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

    /// Multi-prereq case (case b in the auto-unblock spec): a
    /// dependent with N gating prereqs auto-unblocks only after the
    /// LAST one reaches `done`. Marking N-1 prereqs done must leave
    /// the dependent in `blocked`; the final transition is what
    /// flips it to `todo`. Without this, two-prereq chores would
    /// kick off as soon as either side landed and start running on
    /// half-finished context.
    #[test]
    fn dependent_stays_blocked_until_all_multi_prereqs_done() {
        let path = temp_db_path("deps-cascade-multi-prereq");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let prereq_b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let prereq_c = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "C".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq_b.id.clone(),
            relation: None,
        })
        .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq_c.id.clone(),
            relation: None,
        })
        .unwrap();

        // Sanity: dependent is auto-blocked by the engine because at
        // least one prereq is still gating.
        let blocked = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = blocked else { panic!() };
        assert_eq!(t.status, "blocked");
        assert_eq!(t.last_status_actor, "engine");

        // First prereq lands. The dependent must stay blocked
        // because the second one is still gating.
        db.update_work_item(
            &prereq_b.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let still_blocked = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = still_blocked else { panic!() };
        assert_eq!(
            t.status, "blocked",
            "dependent must stay blocked while at least one prereq is still gating",
        );
        assert_eq!(t.last_status_actor, "engine");

        // Last prereq lands. NOW the dependent flips to `todo`.
        db.update_work_item(
            &prereq_c.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let unblocked = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = unblocked else { panic!() };
        assert_eq!(t.status, "todo", "all prereqs done — dependent must auto-unblock");
        assert_eq!(t.last_status_actor, "engine");
        let _ = std::fs::remove_file(path);
    }

    /// Regression case (case c in the auto-unblock spec): once a
    /// dependent has been auto-unblocked, dragging the prereq
    /// backwards out of `done` (e.g. someone realised it wasn't
    /// done after all and moved it back to `backlog`/`todo`) must
    /// NOT yank the dependent back to `blocked`. The dependent may
    /// already be running or in `in_review`; re-blocking it would
    /// lose state. The dispatcher's gating check is the safety net —
    /// a regressed prereq immediately re-gates any future dispatch
    /// of its dependents.
    #[test]
    fn prereq_regression_does_not_re_block_dependents() {
        let path = temp_db_path("deps-cascade-regression");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let prereq = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        // Drive the prereq to `done` so the dependent auto-unblocks
        // to `todo`.
        db.update_work_item(
            &prereq.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let unblocked = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = unblocked else { panic!() };
        assert_eq!(t.status, "todo");
        assert_eq!(t.last_status_actor, "engine");

        // Regression: prereq goes back to `backlog`. The dependent
        // must stay where it is (`todo`), NOT slide back to
        // `blocked`. The dispatcher will refuse to launch it via the
        // separate `gating_prereqs_for` gate.
        db.update_work_item(
            &prereq.id,
            WorkItemPatch {
                status: Some("backlog".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let after_regression = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = after_regression else { panic!() };
        assert_eq!(
            t.status, "todo",
            "prereq regressing out of `done` must NOT yank the dependent back to `blocked`",
        );
        // The dispatcher gate must still see the regressed prereq as
        // gating, so a future RequestExecution against the dependent
        // is refused even though the kanban shows it in `todo`.
        let conn = db.connect().unwrap();
        let gating = deps::gating_prereqs_for(&conn, &dependent.id).unwrap();
        assert_eq!(
            gating, [prereq.id.clone()],
            "regressed prereq must re-appear in gating_prereqs_for",
        );
        let _ = std::fs::remove_file(path);
    }

    /// Hardening case (case d in the auto-unblock spec): a cyclic
    /// edge graph (only constructible by bypassing the engine's
    /// `would_create_cycle` check, e.g. raw SQL) must not cause the
    /// cascade to loop. The cascade walks `dependents_of` exactly
    /// one step; recursion is the dispatcher's job, not the
    /// transition cascade's. This test inserts a cycle directly
    /// into the DB and confirms `mark_chore_pr_merged` returns
    /// without spinning forever.
    #[test]
    fn cyclic_edges_do_not_loop_the_cascade() {
        let path = temp_db_path("deps-cascade-cycle");
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        // Insert both edges directly to bypass `would_create_cycle`
        // and forge a 2-cycle. (The engine refuses to create this
        // shape via `add_dependency`, but a corrupted DB or future
        // schema change could still produce it; the cascade must be
        // robust regardless.)
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_item_dependencies
                (dependent_id, prerequisite_id, relation, created_at)
             VALUES (?1, ?2, 'blocks', '0'), (?2, ?1, 'blocks', '0')",
            rusqlite::params![a.id, b.id],
        )
        .unwrap();
        drop(conn);

        // Drive B to `done` via the merge poller's path — the same
        // entry point the production bug reported. Must return
        // promptly; if the cascade looped, this would hang.
        let updated = db.mark_chore_pr_merged(&b.id, "https://example.test/pr/1").unwrap();
        assert!(
            updated.is_some(),
            "mark_chore_pr_merged should report a transition for the cycle prereq",
        );
        let b_after = db.get_work_item(&b.id).unwrap();
        let WorkItem::Chore(t) = b_after else { panic!() };
        assert_eq!(t.status, "done");
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
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

    /// Regression: a task stuck in `blocked` with `blocked_reason='dependency'`
    /// whose prereq is now `done` must be dispatchable via `RequestExecution`.
    /// This covers the user-override path (kanban drag-to-Doing / bossctl
    /// work start) when the auto-unblock cascade failed to fire — e.g. because
    /// a subsequent update reset `last_status_actor` to `'human'`.
    ///
    /// The fix in `request_execution_in_tx_with_live_check` re-evaluates
    /// prereqs on the verb, clears the stale block, and creates a `ready`
    /// execution so the dispatcher can proceed.
    #[test]
    fn request_execution_clears_stale_dependency_block_when_prereqs_done() {
        let path = temp_db_path("deps-clear-stale-block");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let prereq = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "prereq".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "dependent".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // Add the edge: dependent is gated by prereq.
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();
        // dependent is now auto-blocked by the engine.
        let dep_after_add = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(t) = dep_after_add else { panic!() };
        assert_eq!(t.status, "blocked", "engine should have auto-blocked dependent");
        assert_eq!(t.blocked_reason.as_deref(), Some("dependency"));

        // Mark prereq done — the cascade should auto-unblock dependent.
        // But simulate a scenario where the cascade failed: manually
        // force last_status_actor back to 'human' on the dependent
        // (mimicking a subsequent update_work_item call that reset it).
        db.connect()
            .unwrap()
            .execute(
                // Clear blocked_reason so the cascade guard falls through to the
                // actor check (None => actor == "engine"). With blocked_reason =
                // 'dependency' still set, the new guard unconditionally unblocks
                // regardless of actor — nulling it out simulates the "stale block"
                // scenario where a human edit already cleared the reason.
                "UPDATE tasks SET last_status_actor = 'human', blocked_reason = NULL WHERE id = ?1",
                [&dependent.id],
            )
            .unwrap();

        // Complete the prereq. The cascade fires but skips dependent
        // because last_status_actor='human' (and blocked_reason is NULL).
        db.update_work_item(
            &prereq.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        // Verify the dependent is still stuck (cascade was skipped).
        // blocked_reason is NULL because we cleared it above to simulate
        // the stale-block scenario.
        let still_stuck = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(stuck) = still_stuck else { panic!() };
        assert_eq!(stuck.status, "blocked", "cascade skipped — still stuck");
        assert_eq!(stuck.blocked_reason, None);

        // RequestExecution (the user-override path) must succeed and
        // clear the stale block.
        let execution = db
            .request_execution(RequestExecutionInput {
                work_item_id: dependent.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
            .expect("RequestExecution should succeed when all prereqs are done");

        assert_eq!(execution.status, "ready", "execution must be ready");

        // The task's kanban status must be cleared to 'todo' so
        // start_execution_run can advance it to 'active'.
        let dep_final = db.get_work_item(&dependent.id).unwrap();
        let WorkItem::Chore(final_task) = dep_final else { panic!() };
        assert_eq!(
            final_task.status, "todo",
            "blocked_reason=dependency must be cleared to todo on RequestExecution"
        );
        assert!(
            final_task.blocked_reason.is_none(),
            "blocked_reason must be NULL after clearing"
        );
        let _ = std::fs::remove_file(path);
    }

    /// Pre-v3 / pre-v4 databases should pick up the new dependency
    /// table and `last_status_actor` columns transparently on open;
    /// the engine writes the latest `schema_version`.
    #[test]
    fn migration_from_pre_v4_adds_deps_table_and_actor_columns() {
        let path = disk_db_path("deps-migrate");
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
        assert_eq!(version, "12");
        let _ = std::fs::remove_file(path);
    }

    /// Pre-existing databases (whose `projects` table predates the
    /// design-doc pointer chore) should pick up the three new
    /// nullable columns transparently on open, and `query_project`
    /// should keep working — every existing row reads back with
    /// `None` on each pointer field.
    #[test]
    fn migration_adds_project_design_doc_columns() {
        let path = disk_db_path("design-doc-migrate");
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
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium');
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_legacy', 'Legacy', 'legacy', 'active', '1700000000', '1700000000');
             INSERT INTO projects(id, product_id, name, slug, status, priority, created_at, updated_at)
             VALUES ('proj_legacy', 'prod_legacy', 'Legacy', 'legacy', 'planned', 'medium', '1700000000', '1700000000');
             INSERT INTO metadata(key, value) VALUES ('schema_version','4');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "projects", "design_doc_repo_remote_url").unwrap());
        assert!(table_has_column(&conn, "projects", "design_doc_branch").unwrap());
        assert!(table_has_column(&conn, "projects", "design_doc_path").unwrap());
        drop(conn);

        let project = query_project(&db.connect().unwrap(), "proj_legacy")
            .unwrap()
            .expect("legacy project should survive migration");
        assert_eq!(project.design_doc_repo_remote_url, None);
        assert_eq!(project.design_doc_branch, None);
        assert_eq!(project.design_doc_path, None);
        let _ = std::fs::remove_file(path);
    }

    /// Round-trip: stamping `created_via` on the input is preserved
    /// across insert + read; omitting it lands `unknown` (the engine-
    /// app handler is responsible for substituting a transport hint
    /// before reaching this layer); the auto-created project design
    /// task is always `engine_auto`.
    #[test]
    fn create_via_round_trip_per_source() {
        let path = temp_db_path("created-via");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "P".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();

        let cli_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "from cli".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: Some(boss_protocol::CREATED_VIA_CLI.to_owned()),
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(cli_chore.created_via, boss_protocol::CREATED_VIA_CLI);

        let unknown_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "no source".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(unknown_chore.created_via, CREATED_VIA_UNKNOWN);

        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Proj".to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        let project_tasks = db.list_tasks(&product.id, Some(&project.id), None).unwrap();
        let design_task = project_tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("project create should auto-spawn a design task");
        assert_eq!(design_task.created_via, CREATED_VIA_ENGINE_AUTO);

        let _ = std::fs::remove_file(path);
    }

    /// Pre-existing databases that predate `created_via` should pick
    /// up the new column with `unknown` for every row, and fresh
    /// writes that follow continue to set their own value.
    #[test]
    fn migration_adds_created_via_with_unknown_default() {
        let path = disk_db_path("created-via-migrate");
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
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium');
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_legacy', 'L', 'l', 'active', '1700000000', '1700000000');
             INSERT INTO tasks(id, product_id, project_id, kind, name, description,
                 status, ordinal, pr_url, deleted_at, created_at, updated_at,
                 autostart, last_status_actor, priority)
             VALUES ('task_legacy', 'prod_legacy', NULL, 'chore', 'old', '',
                 'todo', NULL, NULL, NULL, '1700000000', '1700000000',
                 1, 'human', 'medium');
             INSERT INTO metadata(key, value) VALUES ('schema_version','4');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "created_via").unwrap());
        let legacy = query_task(&conn, "task_legacy").unwrap().unwrap();
        assert_eq!(legacy.created_via, CREATED_VIA_UNKNOWN);
        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");
        let _ = std::fs::remove_file(path);
    }

    /// Fresh init (no pre-existing tables) lands the three pointer
    /// columns via `CREATE TABLE`, not via the migration path. Verify
    /// both routes converge on the same schema shape.
    #[test]
    fn fresh_init_includes_project_design_doc_columns() {
        let path = temp_db_path("design-doc-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "projects", "design_doc_repo_remote_url").unwrap());
        assert!(table_has_column(&conn, "projects", "design_doc_branch").unwrap());
        assert!(table_has_column(&conn, "projects", "design_doc_path").unwrap());
        let _ = std::fs::remove_file(path);
    }

    /// Fresh init lands the new `tasks.repo_remote_url` column, the
    /// partial `tasks_repo_idx` index, and bumps the recorded
    /// `schema_version` to the current value.
    #[test]
    fn fresh_init_includes_tasks_repo_remote_url() {
        let path = temp_db_path("tasks-repo-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        assert!(table_has_column(&conn, "tasks", "repo_remote_url").unwrap());

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'tasks_repo_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");

        let _ = std::fs::remove_file(path);
    }

    /// A pre-v5 database (no `repo_remote_url` column on `tasks`,
    /// `schema_version = 4`) should pick up the new column with
    /// existing rows defaulting to `NULL`, get the partial index
    /// created, and have `schema_version` bumped to the current
    /// value.
    #[test]
    fn migration_from_v4_adds_tasks_repo_remote_url() {
        let path = disk_db_path("tasks-repo-migrate");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Stand up a minimal v4 schema: just enough to round-trip a
        // single task row that pre-dates the new column.
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
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 design_doc_repo_remote_url TEXT,
                 design_doc_branch TEXT,
                 design_doc_path TEXT);
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium');
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_legacy', 'Legacy', 'legacy', 'active',
                     '1700000000', '1700000000');
             INSERT INTO tasks(id, product_id, kind, name, status,
                               created_at, updated_at)
             VALUES ('task_legacy', 'prod_legacy', 'chore', 'Legacy',
                     'todo', '1700000000', '1700000000');
             INSERT INTO metadata(key, value) VALUES ('schema_version','4');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        // New column lands and the legacy row reads back as NULL.
        assert!(table_has_column(&conn, "tasks", "repo_remote_url").unwrap());
        let legacy_repo: Option<String> = conn
            .query_row(
                "SELECT repo_remote_url FROM tasks WHERE id = 'task_legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_repo, None);

        // Partial index materializes on the migration path too — the
        // index DDL only runs once the column exists, so a pre-v5
        // database that fails to migrate would also fail this check.
        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'tasks_repo_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        // schema_version moves from 4 → current.
        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");

        let _ = std::fs::remove_file(path);
    }

    /// Schema v7 migration: a pre-v7 `work_attention_items` table
    /// (NOT NULL `execution_id`, no `work_item_id`) is rebuilt in
    /// place. Existing rows survive with their `execution_id` intact
    /// and a `NULL` `work_item_id`, the new column lands, and the
    /// CHECK constraint accepts work-item-scoped writes afterwards.
    #[test]
    fn migration_v6_to_v7_relaxes_work_attention_items() {
        let path = disk_db_path("attn-v7-migrate");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Stand up just enough of the v6 schema to land an existing
        // attention item against an execution row. Everything else
        // the `WorkDb::open` migration touches will be created via
        // the fresh-init path when it doesn't already exist.
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
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium');
             CREATE TABLE work_executions (
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
                 finished_at TEXT);
             CREATE TABLE work_attention_items (
                 id TEXT PRIMARY KEY,
                 execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 status TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body_markdown TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 resolved_at TEXT);
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
                 VALUES ('prod_legacy', 'L', 'l', 'active', '1700000000', '1700000000');
             INSERT INTO tasks(id, product_id, project_id, kind, name, status,
                               created_at, updated_at)
                 VALUES ('task_legacy', 'prod_legacy', NULL, 'chore', 'old',
                         'todo', '1700000000', '1700000000');
             INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url,
                                          created_at)
                 VALUES ('exec_legacy', 'task_legacy', 'chore_implementation', 'ready',
                         'git@github.com:legacy/repo.git', '1700000000');
             INSERT INTO work_attention_items(id, execution_id, kind, status, title,
                                              body_markdown, created_at)
                 VALUES ('attn_legacy', 'exec_legacy', 'review_required', 'open',
                         'Legacy item', 'Body.', '1700000000');
             INSERT INTO metadata(key, value) VALUES ('schema_version','6');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "work_attention_items", "work_item_id").unwrap());
        let (exec_id, work_item_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT execution_id, work_item_id FROM work_attention_items WHERE id = 'attn_legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(exec_id.as_deref(), Some("exec_legacy"));
        assert_eq!(work_item_id, None);

        // The new code path accepts a work-item-scoped insert,
        // proving the CHECK constraint is the relaxed v7 shape.
        let new_attn = db
            .create_attention_item(CreateAttentionItemInput {
                execution_id: None,
                work_item_id: Some("task_legacy".to_owned()),
                kind: "repo_unresolved".to_owned(),
                status: None,
                title: "T".to_owned(),
                body_markdown: "B".to_owned(),
                resolved_at: None,
            })
            .unwrap();
        assert_eq!(new_attn.execution_id, None);
        assert_eq!(new_attn.work_item_id.as_deref(), Some("task_legacy"));

        let _ = std::fs::remove_file(path);
    }

    /// Round-trip the conflict-resolution attempt lifecycle: insert
    /// → set diagnosis → mark running → mark failed. Covers the new
    /// WorkDb surface that the worker spawn flow + the worker-facing
    /// `boss engine conflicts mark-failed` CLI both depend on.
    #[test]
    fn conflict_resolution_round_trip() {
        let path = temp_db_path("conflict-resolution-rt");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "P".into(),
                description: None,
                repo_remote_url: Some("git@example.invalid:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "C".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // Flip the chore into `blocked: merge_conflict` so the
        // insert path's UPDATE-tasks side stamps blocked_attempt_id.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/foo/bar/pull/42".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&chore.id, "https://github.com/foo/bar/pull/42")
            .unwrap();

        let attempt = db
            .insert_conflict_resolution(super::ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: "https://github.com/foo/bar/pull/42".into(),
                pr_number: 42,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("abc123".into()),
                head_sha_before: Some("def456".into()),
            })
            .unwrap()
            .expect("first insert must produce a row");
        assert_eq!(attempt.status, "pending");
        assert_eq!(attempt.pr_number, 42);
        assert_eq!(attempt.head_branch, "feature");
        assert_eq!(attempt.base_sha_at_trigger.as_deref(), Some("abc123"));

        // Parent's blocked_attempt_id is stamped to the new attempt.
        let task = db.get_work_item(&chore.id).unwrap();
        match task {
            WorkItem::Chore(t) => {
                assert_eq!(t.blocked_attempt_id.as_deref(), Some(attempt.id.as_str()));
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Idempotent on the (work_item_id, base_sha) key.
        let second = db
            .insert_conflict_resolution(super::ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: "https://github.com/foo/bar/pull/42".into(),
                pr_number: 42,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("abc123".into()),
                head_sha_before: Some("def456".into()),
            })
            .unwrap();
        assert!(second.is_none(), "second insert on same key must be a no-op");

        // Active-attempt lookup returns the pending row.
        let active = db
            .active_conflict_resolution_for_work_item(&chore.id)
            .unwrap()
            .expect("pending attempt should be active");
        assert_eq!(active.id, attempt.id);

        // Diagnosis store / read.
        let json = r#"{"schema_version":1,"base_sha":"abc","head_sha":"def","files":[],"error":null}"#;
        let stored = db
            .set_conflict_resolution_diagnosis(&attempt.id, json)
            .unwrap()
            .expect("diagnosis update returns updated row");
        assert_eq!(stored.conflict_diagnosis.as_deref(), Some(json));

        // pending → running.
        let running = db
            .mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
            .unwrap()
            .expect("running flip returns updated row");
        assert_eq!(running.status, "running");
        assert_eq!(running.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(running.cube_workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(running.worker_id.as_deref(), Some("worker-1"));
        assert!(running.started_at.is_some());

        // running → failed via the worker-facing surface.
        let failed = db
            .mark_conflict_resolution_failed(&attempt.id, "obsolescence_suspected")
            .unwrap()
            .expect("failure flip returns updated row");
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.failure_reason.as_deref(),
            Some("obsolescence_suspected"),
        );
        assert!(failed.finished_at.is_some());

        // mark_failed is idempotent on terminal rows — second call
        // matches no rows and returns Ok(None).
        let again = db
            .mark_conflict_resolution_failed(&attempt.id, "redundant")
            .unwrap();
        assert!(
            again.is_none(),
            "second mark-failed on terminal row must be a no-op",
        );

        // Unknown attempt id → Ok(None).
        let missing = db
            .mark_conflict_resolution_failed("crz_does_not_exist", "x")
            .unwrap();
        assert!(missing.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// Fresh init lands the merge-conflict-handling columns and the
    /// `conflict_resolutions` side table. Phase 1 of the
    /// merge-conflict-handling design.
    #[test]
    fn fresh_init_includes_merge_conflict_schema() {
        let path = temp_db_path("mc-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "blocked_reason").unwrap());
        assert!(table_has_column(&conn, "tasks", "blocked_attempt_id").unwrap());
        assert!(
            table_has_column(&conn, "products", "auto_pr_maintenance_enabled").unwrap(),
            "products should carry the unified opt-out flag after a fresh init",
        );
        let table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'conflict_resolutions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 1, "conflict_resolutions table should exist");
        let _ = std::fs::remove_file(path);
    }

    /// A pre-v6 database with the original `products.auto_rebase_enabled`
    /// column should have it renamed in place to
    /// `auto_pr_maintenance_enabled`, preserving any existing value.
    /// Idempotent across re-opens.
    #[test]
    fn migration_renames_auto_rebase_enabled_when_present() {
        let path = disk_db_path("mc-rename-auto-rebase");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE products (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                 description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
                 status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 auto_rebase_enabled INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE projects (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, name TEXT NOT NULL,
                 slug TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                 goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium',
                 created_via TEXT NOT NULL DEFAULT 'unknown');
             INSERT INTO products(id, name, slug, status, created_at, updated_at,
                                   auto_rebase_enabled)
             VALUES ('prod_legacy', 'L', 'l', 'active', '1700000000', '1700000000', 0);
             INSERT INTO metadata(key, value) VALUES ('schema_version','5');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(
            !table_has_column(&conn, "products", "auto_rebase_enabled").unwrap(),
            "old column should have been renamed",
        );
        assert!(
            table_has_column(&conn, "products", "auto_pr_maintenance_enabled").unwrap(),
            "new column should be present after rename",
        );
        let preserved: i64 = conn
            .query_row(
                "SELECT auto_pr_maintenance_enabled FROM products WHERE id = 'prod_legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 0, "existing value must carry across the rename");

        // Idempotency: re-opening must be a no-op (no double-rename
        // panic on an already-renamed column).
        drop(conn);
        let db2 = WorkDb::open(path.clone()).unwrap();
        let conn2 = db2.connect().unwrap();
        assert!(table_has_column(&conn2, "products", "auto_pr_maintenance_enabled").unwrap());
        let _ = std::fs::remove_file(path);
    }

    /// Backfill flips `blocked_reason = 'dependency'` for an existing
    /// `blocked` row that is gated by a still-incomplete prereq edge,
    /// and leaves `NULL` on a `blocked` row whose prereq is already
    /// `done` (legacy human-set block).
    #[test]
    fn migration_backfills_blocked_reason_for_active_prereqs() {
        let path = disk_db_path("mc-blocked-backfill");
        // Stand up the pre-Phase-1 schema by hand: needs `tasks`,
        // `projects`, `products`, `work_item_dependencies`, plus
        // `last_status_actor`. We pre-seed two blocked dependents,
        // one whose prereq is still `todo` (should backfill to
        // `'dependency'`) and one whose prereq is already `done`
        // (should stay `NULL`).
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
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium',
                 created_via TEXT NOT NULL DEFAULT 'unknown');
             CREATE TABLE work_item_dependencies (
                 dependent_id TEXT NOT NULL, prerequisite_id TEXT NOT NULL,
                 relation TEXT NOT NULL DEFAULT 'blocks',
                 created_at TEXT NOT NULL,
                 PRIMARY KEY (dependent_id, prerequisite_id, relation));
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_1', 'P', 'p', 'active', '1700000000', '1700000000');
             -- Two dependents already in `blocked` from the engine's
             -- pre-Phase-1 auto-block path; both still have edges.
             INSERT INTO tasks(id, product_id, project_id, kind, name,
                                description, status, created_at, updated_at,
                                autostart, last_status_actor, priority, created_via)
             VALUES
              ('task_dep_active',   'prod_1', NULL, 'chore', 'gated by todo',
               '', 'blocked', '1700000000', '1700000000', 1, 'engine', 'medium', 'cli'),
              ('task_dep_done',     'prod_1', NULL, 'chore', 'gated by done',
               '', 'blocked', '1700000000', '1700000000', 1, 'engine', 'medium', 'cli'),
              ('task_prereq_todo',  'prod_1', NULL, 'chore', 'still todo',
               '', 'todo',    '1700000000', '1700000000', 1, 'human',  'medium', 'cli'),
              ('task_prereq_done',  'prod_1', NULL, 'chore', 'already done',
               '', 'done',    '1700000000', '1700000000', 1, 'human',  'medium', 'cli'),
              ('task_legacy_block', 'prod_1', NULL, 'chore', 'manual block',
               '', 'blocked', '1700000000', '1700000000', 1, 'human',  'medium', 'cli');
             INSERT INTO work_item_dependencies(dependent_id, prerequisite_id, relation, created_at)
             VALUES
              ('task_dep_active', 'task_prereq_todo', 'blocks', '1700000000'),
              ('task_dep_done',   'task_prereq_done', 'blocks', '1700000000');
             INSERT INTO metadata(key, value) VALUES ('schema_version','5');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        let reason_for = |id: &str| -> Option<String> {
            conn.query_row(
                "SELECT blocked_reason FROM tasks WHERE id = ?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert_eq!(
            reason_for("task_dep_active").as_deref(),
            Some("dependency"),
            "blocked row with non-done prereq should be backfilled",
        );
        assert_eq!(
            reason_for("task_dep_done"),
            None,
            "blocked row whose prereq already finished is a legacy manual block",
        );
        assert_eq!(
            reason_for("task_legacy_block"),
            None,
            "blocked row with no edges stays legacy NULL",
        );

        // Idempotency: re-opening leaves the same values in place.
        drop(conn);
        let db2 = WorkDb::open(path.clone()).unwrap();
        let conn2 = db2.connect().unwrap();
        let count_dependency: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE blocked_reason = 'dependency'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_dependency, 1);
        let _ = std::fs::remove_file(path);
    }

    /// Fresh init lands the CI Phase-7 schema: the multi-signal side
    /// table, the `ci_remediations` + `ci_failure_suppressions`
    /// tables, the per-PR budget columns on `tasks`, and the
    /// product-level budget default on `products`. Pairs with
    /// [`fresh_init_includes_merge_conflict_schema`] for the
    /// Phase-1 columns.
    #[test]
    fn fresh_init_includes_ci_phase7_schema() {
        let path = temp_db_path("ci-p7-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        for table in [
            "task_blocked_signals",
            "ci_remediations",
            "ci_failure_suppressions",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} table should exist after fresh init");
        }
        assert!(table_has_column(&conn, "tasks", "ci_attempt_budget").unwrap());
        assert!(table_has_column(&conn, "tasks", "ci_attempts_used").unwrap());
        assert!(table_has_column(&conn, "products", "ci_attempt_budget").unwrap());

        // The fresh-init `CREATE TABLE` for products carries the
        // design's documented default budget (3). A product inserted
        // through the normal path picks that up without the caller
        // having to set it.
        let product = db
            .create_product(CreateProductInput {
                name: "P".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let budget: i64 = conn
            .query_row(
                "SELECT ci_attempt_budget FROM products WHERE id = ?1",
                [&product.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(budget, 3, "product budget must default to 3 on fresh init");

        let _ = std::fs::remove_file(path);
    }

    /// Fresh-init must include the `effort_escalations` side table
    /// the audit report reads (design §Q4 follow-up, PR #370). New
    /// databases get the table directly; legacy databases pick it
    /// up via [`migrate_effort_escalations_table`] on first open.
    #[test]
    fn fresh_init_includes_effort_escalations_table() {
        let path = temp_db_path("effort-escalations-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'effort_escalations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        // Every column the audit / record paths touch must exist
        // so we can fail fast on a partial migration.
        for column in [
            "id",
            "product_id",
            "work_item_id",
            "original_level",
            "new_level",
            "markers",
            "rule_id",
            "created_at",
        ] {
            assert!(
                table_has_column(&conn, "effort_escalations", column).unwrap(),
                "missing column {column}",
            );
        }
        let _ = std::fs::remove_file(path);
    }

    /// Record-then-read round-trips through the engine API the
    /// audit report depends on. Confirms the row encodes the
    /// markers JSON array intact, that the level enums survive a
    /// FromStr / Display round-trip, and that the listing query
    /// filters by `product_id` and applies the window cutoff.
    #[test]
    fn record_and_list_effort_escalations_round_trip() {
        let path = temp_db_path("effort-escalations-rt");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Rename helper".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let chore_id = chore.id.clone();
        let recorded = db
            .record_effort_escalation(
                &chore_id,
                EffortLevel::Trivial,
                EffortLevel::Small,
                &["rename".to_owned(), "cursor".to_owned()],
                Some("rule-5"),
            )
            .unwrap();
        assert_eq!(recorded.product_id, product.id);
        assert_eq!(recorded.work_item_id, chore_id);
        assert_eq!(recorded.markers, vec!["rename", "cursor"]);
        assert_eq!(recorded.rule_id.as_deref(), Some("rule-5"));
        assert!(recorded.id.starts_with("esc_"));

        let listed = db
            .list_effort_escalations_for_product(&product.id, None)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, recorded.id);
        assert_eq!(listed[0].markers, vec!["rename", "cursor"]);

        // A window cutoff in the future drops the row.
        let listed_far_future = db
            .list_effort_escalations_for_product(&product.id, Some(i64::MAX / 2))
            .unwrap();
        assert!(listed_far_future.is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// Fresh init must include the external-tracker columns on `products`
    /// and `tasks`, plus the two partial indices. Migration from a
    /// pre-existing schema must add the same columns idempotently.
    #[test]
    fn fresh_init_includes_external_tracker_schema() {
        let path = temp_db_path("ext-tracker-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        for column in ["external_tracker_kind", "external_tracker_config"] {
            assert!(
                table_has_column(&conn, "products", column).unwrap(),
                "missing products.{column}",
            );
        }
        for column in [
            "external_ref_kind",
            "external_ref_canonical_id",
            "external_ref_raw",
            "external_ref_synced_at",
            "external_ref_unbound_at",
        ] {
            assert!(
                table_has_column(&conn, "tasks", column).unwrap(),
                "missing tasks.{column}",
            );
        }
        // Both partial indices must be present.
        for idx in [
            "tasks_external_ref_idx",
            "tasks_external_ref_bound_uniq",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'index' AND name = ?1",
                    [idx],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "index {idx} should exist after fresh init");
        }
        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");
        let _ = std::fs::remove_file(path);
    }

    /// A database without the external-tracker columns must pick them
    /// up on migration and the unique partial index must reject a
    /// duplicate bound row while allowing the same canonical_id when
    /// one row is unbound (`external_ref_unbound_at IS NOT NULL`).
    #[test]
    fn migration_adds_external_tracker_columns_and_unique_index_enforced() {
        let path = disk_db_path("ext-tracker-migrate");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE products (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                 description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
                 status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 priority TEXT NOT NULL DEFAULT 'medium',
                 created_via TEXT NOT NULL DEFAULT 'unknown');
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_1', 'P', 'p', 'active', '1700000000', '1700000000');
             INSERT INTO metadata(key, value) VALUES ('schema_version','4');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        for column in ["external_tracker_kind", "external_tracker_config"] {
            assert!(
                table_has_column(&conn, "products", column).unwrap(),
                "migration must add products.{column}",
            );
        }
        for column in [
            "external_ref_kind",
            "external_ref_canonical_id",
            "external_ref_raw",
            "external_ref_synced_at",
            "external_ref_unbound_at",
        ] {
            assert!(
                table_has_column(&conn, "tasks", column).unwrap(),
                "migration must add tasks.{column}",
            );
        }

        // The unique partial index rejects two simultaneously-bound rows.
        conn.execute(
            "INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at,
                               external_ref_kind, external_ref_canonical_id)
             VALUES ('t1', 'prod_1', 'chore', 'A', 'todo', '1700000001', '1700000001',
                     'github', 'spinyfin/mono#1')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at,
                               external_ref_kind, external_ref_canonical_id)
             VALUES ('t2', 'prod_1', 'chore', 'B', 'todo', '1700000002', '1700000002',
                     'github', 'spinyfin/mono#1')",
            [],
        );
        assert!(
            dup.is_err(),
            "duplicate bound canonical_id must violate unique index",
        );

        // The same canonical_id is allowed when one row is unbound.
        conn.execute(
            "UPDATE tasks SET external_ref_unbound_at = '1700000100' WHERE id = 't1'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at,
                               external_ref_kind, external_ref_canonical_id)
             VALUES ('t3', 'prod_1', 'chore', 'C', 'todo', '1700000003', '1700000003',
                     'github', 'spinyfin/mono#1')",
            [],
        )
        .unwrap();

        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");
        let _ = std::fs::remove_file(path);
    }

    // ── T8 WorkDb external-ref method tests ─────────────────────────────────

    /// Helper: create a product and a chore in a fresh in-memory db.
    /// Returns `(db, product_id, chore_id)`.
    fn setup_product_and_chore() -> (WorkDb, String, String) {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "TestProduct".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Fix thing".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        (db, product.id, chore.id)
    }

    /// set_external_ref writes the columns; find_by_external_ref returns
    /// the row with external_ref populated.
    #[test]
    fn set_and_find_external_ref_round_trip() {
        let (db, _product_id, chore_id) = setup_product_and_chore();
        let raw = serde_json::json!({ "issue_number": 560, "project_item_id": "PVT_abc" });
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#560", &raw)
            .unwrap();

        let task = db
            .find_by_external_ref("github", "spinyfin/mono#560")
            .unwrap()
            .expect("must find the row");
        assert_eq!(task.id, chore_id);
        let ext = task.external_ref.expect("external_ref must be populated");
        assert_eq!(ext.kind, "github");
        assert_eq!(ext.canonical_id, "spinyfin/mono#560");
        assert_eq!(ext.raw["issue_number"], 560);
        assert_eq!(ext.unbound_at, None);
    }

    /// set_external_ref on a work item that already has a binding replaces
    /// it silently (update semantics).
    #[test]
    fn set_external_ref_replaces_existing_binding() {
        let (db, _product_id, chore_id) = setup_product_and_chore();
        let raw1 = serde_json::json!({ "issue_number": 1 });
        let raw2 = serde_json::json!({ "issue_number": 2 });
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#1", &raw1)
            .unwrap();
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#2", &raw2)
            .unwrap();

        assert!(
            db.find_by_external_ref("github", "spinyfin/mono#1")
                .unwrap()
                .is_none(),
            "old canonical_id must no longer match"
        );
        let task = db
            .find_by_external_ref("github", "spinyfin/mono#2")
            .unwrap()
            .expect("new canonical_id must match");
        assert_eq!(task.id, chore_id);
    }

    /// clear_external_ref sets unbound_at; find_by_external_ref then
    /// returns None for that canonical_id.
    #[test]
    fn clear_external_ref_hides_from_find() {
        let (db, _product_id, chore_id) = setup_product_and_chore();
        let raw = serde_json::json!({});
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#10", &raw)
            .unwrap();
        db.clear_external_ref(&chore_id).unwrap();

        let found = db
            .find_by_external_ref("github", "spinyfin/mono#10")
            .unwrap();
        assert!(found.is_none(), "cleared row must not appear in find_by_external_ref");
    }

    /// After clear, set_external_ref on the same row (rebind from unbound
    /// state) resets unbound_at and makes the row findable again.
    #[test]
    fn rebind_from_unbound_state() {
        let (db, _product_id, chore_id) = setup_product_and_chore();
        let raw = serde_json::json!({ "issue_number": 99 });
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#99", &raw)
            .unwrap();
        db.clear_external_ref(&chore_id).unwrap();
        // Rebind.
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#99", &raw)
            .unwrap();

        let task = db
            .find_by_external_ref("github", "spinyfin/mono#99")
            .unwrap()
            .expect("rebind must make the row findable again");
        let ext = task.external_ref.unwrap();
        assert_eq!(ext.unbound_at, None, "unbound_at must be cleared on rebind");
    }

    /// The unique partial index rejects two simultaneously-bound rows for
    /// the same (kind, canonical_id) while the same canonical_id is
    /// allowed when one row is unbound.
    #[test]
    fn unique_index_rejects_duplicate_bound_rows() {
        let (db, product_id, chore_id) = setup_product_and_chore();
        let chore2 = db
            .create_chore(CreateChoreInput {
                product_id: product_id.clone(),
                name: "Second chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let raw = serde_json::json!({});
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#77", &raw)
            .unwrap();
        // Binding the same canonical_id to a second chore while the first
        // is still bound must fail (unique partial index violation).
        let err = db.set_external_ref(&chore2.id, "github", "spinyfin/mono#77", &raw);
        assert!(err.is_err(), "duplicate bound binding must be rejected");

        // After unbinding the first, the second bind must succeed.
        db.clear_external_ref(&chore_id).unwrap();
        db.set_external_ref(&chore2.id, "github", "spinyfin/mono#77", &raw)
            .unwrap();
        let task = db
            .find_by_external_ref("github", "spinyfin/mono#77")
            .unwrap()
            .expect("second bind after unbind must be findable");
        assert_eq!(task.id, chore2.id);
    }

    /// list_external_refs_for_product returns both bound and unbound rows
    /// (those with external_ref_canonical_id IS NOT NULL).
    #[test]
    fn list_external_refs_includes_unbound_rows() {
        let (db, product_id, chore_id) = setup_product_and_chore();
        let chore2 = db
            .create_chore(CreateChoreInput {
                product_id: product_id.clone(),
                name: "Another chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        let raw = serde_json::json!({ "n": 1 });
        db.set_external_ref(&chore_id, "github", "spinyfin/mono#1", &raw)
            .unwrap();
        db.set_external_ref(&chore2.id, "github", "spinyfin/mono#2", &raw)
            .unwrap();
        db.clear_external_ref(&chore_id).unwrap();

        let refs = db.list_external_refs_for_product(&product_id).unwrap();
        assert_eq!(refs.len(), 2, "both bound and unbound rows must appear");

        let ids: Vec<&str> = refs.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&chore_id.as_str()));
        assert!(ids.contains(&chore2.id.as_str()));

        let unbound = refs
            .iter()
            .find(|(id, _)| id == &chore_id)
            .map(|(_, r)| r)
            .unwrap();
        assert!(unbound.unbound_at.is_some(), "cleared row must have unbound_at set");

        let bound = refs
            .iter()
            .find(|(id, _)| id == &chore2.id)
            .map(|(_, r)| r)
            .unwrap();
        assert!(bound.unbound_at.is_none(), "active row must have no unbound_at");
    }

    /// A database that does not yet have the external-tracker columns must
    /// return an empty list from list_external_refs_for_product (migration
    /// adds the columns with NULL defaults, so no rows qualify).
    #[test]
    fn list_external_refs_returns_empty_when_no_refs_set() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "NoRefs".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let refs = db
            .list_external_refs_for_product(&product.id)
            .unwrap();
        assert!(refs.is_empty());
    }

    /// A database carrying the Phase-1 merge-conflict schema (but
    /// not yet Phase-7's CI columns) should pick up the new tables
    /// and columns transparently on re-open, and existing rows whose
    /// `blocked_reason` was scalar-only get a mirroring row in the
    /// `task_blocked_signals` side table. Idempotent across re-opens.
    #[test]
    fn migration_from_phase1_adds_ci_phase7_schema_and_backfills_signals() {
        let path = disk_db_path("ci-p7-migrate");
        // Stand up a "MC P1-only" schema: tasks have blocked_reason
        // + blocked_attempt_id (the Phase-1 additions), there's a
        // conflict_resolutions table, but none of the Phase-7
        // tables or columns. We seed two blocked rows so the
        // backfill has something to mirror.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE products (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                 description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
                 status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 auto_pr_maintenance_enabled INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE projects (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, name TEXT NOT NULL,
                 slug TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                 goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 priority TEXT NOT NULL, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 last_status_actor TEXT NOT NULL DEFAULT 'human');
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 priority TEXT NOT NULL DEFAULT 'medium',
                 created_via TEXT NOT NULL DEFAULT 'unknown',
                 blocked_reason TEXT,
                 blocked_attempt_id TEXT);
             CREATE TABLE conflict_resolutions (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL,
                 work_item_id TEXT NOT NULL, pr_url TEXT NOT NULL,
                 pr_number INTEGER NOT NULL, head_branch TEXT NOT NULL,
                 base_branch TEXT NOT NULL,
                 status TEXT NOT NULL, created_at TEXT NOT NULL);
             INSERT INTO products(id, name, slug, status, created_at, updated_at)
             VALUES ('prod_1', 'P', 'p', 'active', '1700000000', '1700000000');
             INSERT INTO tasks(id, product_id, project_id, kind, name, status,
                                created_at, updated_at, autostart,
                                last_status_actor, priority, created_via,
                                blocked_reason, blocked_attempt_id)
             VALUES
              ('task_mc',  'prod_1', NULL, 'chore', 'mc-blocked',
               'blocked', '1700000000', '1700000050',
               1, 'engine', 'medium', 'cli',
               'merge_conflict', 'conflict_18ab_1'),
              ('task_dep', 'prod_1', NULL, 'chore', 'dep-blocked',
               'blocked', '1700000000', '1700000060',
               1, 'engine', 'medium', 'cli',
               'dependency', NULL),
              ('task_clean', 'prod_1', NULL, 'chore', 'not-blocked',
               'in_review', '1700000000', '1700000070',
               1, 'human', 'medium', 'cli', NULL, NULL),
              ('task_deleted', 'prod_1', NULL, 'chore', 'soft-deleted',
               'blocked', '1700000000', '1700000080',
               1, 'engine', 'medium', 'cli',
               'merge_conflict', 'conflict_18ab_2');
             -- The soft-deleted row should NOT be backfilled.
             UPDATE tasks SET deleted_at = '1700000090' WHERE id = 'task_deleted';
             INSERT INTO metadata(key, value) VALUES ('schema_version','7');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        // New tables exist.
        for table in [
            "task_blocked_signals",
            "ci_remediations",
            "ci_failure_suppressions",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} should land via the migration");
        }

        // New columns exist on tasks / products.
        assert!(table_has_column(&conn, "tasks", "ci_attempt_budget").unwrap());
        assert!(table_has_column(&conn, "tasks", "ci_attempts_used").unwrap());
        assert!(table_has_column(&conn, "products", "ci_attempt_budget").unwrap());

        // The product budget column gets the documented default of
        // 3 for the existing row added through the migration path.
        let preserved: i64 = conn
            .query_row(
                "SELECT ci_attempt_budget FROM products WHERE id = 'prod_1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 3);

        // Backfill: each `blocked` row with a non-NULL
        // `blocked_reason` becomes one row in the side table.
        // Soft-deleted rows are excluded; clean rows (no
        // blocked_reason) produce no row.
        let signals: Vec<(String, String, Option<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT work_item_id, reason, attempt_id
                     FROM task_blocked_signals
                     ORDER BY work_item_id",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            signals,
            vec![
                (
                    "task_dep".to_owned(),
                    "dependency".to_owned(),
                    None,
                ),
                (
                    "task_mc".to_owned(),
                    "merge_conflict".to_owned(),
                    Some("conflict_18ab_1".to_owned()),
                ),
            ],
            "backfill must mirror exactly the active, non-deleted \
             blocked rows",
        );

        // Idempotency: a second open is a no-op. Counts stay put
        // and the schema_version stamp is the current value.
        drop(conn);
        let db2 = WorkDb::open(path.clone()).unwrap();
        let conn2 = db2.connect().unwrap();
        let again: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM task_blocked_signals",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(again, 2);
        let version: String = conn2
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "12");

        // After migration we can also write a fresh `blocked` row
        // and re-backfill is still a no-op (the existing rows
        // satisfy the `(work_item_id, reason)` PK so OR IGNORE
        // covers them; the new row is picked up). This is the
        // "engine sweep" simulation from the design's §Q2 note.
        conn2
            .execute(
                "INSERT INTO tasks(id, product_id, project_id, kind, name, status,
                                    created_at, updated_at, autostart,
                                    last_status_actor, priority, created_via,
                                    blocked_reason, blocked_attempt_id)
                 VALUES ('task_ci', 'prod_1', NULL, 'chore', 'ci-blocked',
                         'blocked', '1700001000', '1700001050',
                         1, 'engine', 'medium', 'cli',
                         'ci_failure', 'ci_18ab_1')",
                [],
            )
            .unwrap();
        super::migrate_backfill_task_blocked_signals(&conn2).unwrap();
        let new_signal_reason: String = conn2
            .query_row(
                "SELECT reason FROM task_blocked_signals WHERE work_item_id = 'task_ci'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_signal_reason, "ci_failure");

        let _ = std::fs::remove_file(path);
    }

    /// `ci_remediations` enforces idempotency on its unique key
    /// `(work_item_id, head_sha_at_trigger, attempt_kind)`. A second
    /// insert with the same triplet must fail (so the engine's
    /// `INSERT OR IGNORE` pattern lands one row per probe). A fix
    /// attempt and a retrigger attempt on the same head sha are
    /// distinct, because `attempt_kind` is part of the key.
    #[test]
    fn ci_remediations_unique_key_enforced() {
        let path = disk_db_path("ci-rem-unique");
        let _db = WorkDb::open(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let insert = |id: &str, head: &str, kind: &str| -> rusqlite::Result<usize> {
            conn.execute(
                "INSERT INTO ci_remediations
                     (id, product_id, work_item_id, pr_url, pr_number,
                      head_branch, head_sha_at_trigger, attempt_kind,
                      consumes_budget, failed_checks, status, created_at)
                 VALUES (?1, 'prod_1', 'task_77',
                         'https://github.com/foo/bar/pull/1', 1,
                         'feat/banana', ?2, ?3, 1, '[]', 'pending',
                         '1700000000')",
                params![id, head, kind],
            )
        };
        insert("ci_1", "abc123", "fix").unwrap();
        // Same triplet → unique-key violation.
        let dup = insert("ci_2", "abc123", "fix");
        assert!(
            dup.is_err(),
            "duplicate (item, head_sha, kind) must be rejected",
        );
        // A retrigger attempt on the same head sha is a *separate*
        // row because `attempt_kind` discriminates.
        insert("ci_3", "abc123", "retrigger").unwrap();
        // A fix attempt on a *new* head sha is also separate.
        insert("ci_4", "def456", "fix").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ci_remediations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let _ = std::fs::remove_file(path);
    }

    /// `task_blocked_signals` upserts on `(work_item_id, reason)` —
    /// re-observing the same signal is a no-op via `INSERT OR
    /// IGNORE`, not a duplicate row. A different reason on the same
    /// work item is a separate row (this is the multi-signal case
    /// the side table exists to model).
    #[test]
    fn task_blocked_signals_pk_enforced() {
        let path = disk_db_path("tbs-pk");
        let _db = WorkDb::open(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO task_blocked_signals
                 (work_item_id, reason, attempt_id, created_at)
             VALUES ('task_77', 'ci_failure', 'ci_18ab_1', '1700000000')",
            [],
        )
        .unwrap();
        // Same (item, reason) → PK violation under plain INSERT.
        let dup = conn.execute(
            "INSERT INTO task_blocked_signals
                 (work_item_id, reason, attempt_id, created_at)
             VALUES ('task_77', 'ci_failure', 'ci_18ab_2', '1700000010')",
            [],
        );
        assert!(dup.is_err(), "duplicate (item, reason) must be rejected");
        // INSERT OR IGNORE on the same pair is a silent no-op.
        let or_ignore = conn
            .execute(
                "INSERT OR IGNORE INTO task_blocked_signals
                     (work_item_id, reason, attempt_id, created_at)
                 VALUES ('task_77', 'ci_failure', 'ci_18ab_3', '1700000020')",
                [],
            )
            .unwrap();
        assert_eq!(or_ignore, 0);
        // Different reason → separate row (the multi-signal case).
        conn.execute(
            "INSERT INTO task_blocked_signals
                 (work_item_id, reason, attempt_id, created_at)
             VALUES ('task_77', 'merge_conflict', 'conflict_18ab_1', '1700000030')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_blocked_signals WHERE work_item_id = 'task_77'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let _ = std::fs::remove_file(path);
    }

    /// Stand up a fresh product + project against `path` so the
    /// design-doc pointer tests don't all open-code the same
    /// boilerplate. Returns the project id; the product's repo URL
    /// is the standard `mono` git@ form the rest of the suite uses.
    fn seed_project_for_design_doc(db: &WorkDb) -> (Product, Project) {
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
                name: "Project design doc pointer".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();
        (product, project)
    }

    /// Convenience: rebuild a `set_project_design_doc` input with
    /// just the project id and path filled in. Most pointer tests
    /// only care about the path; defaulting the rest keeps signal
    /// high.
    fn set_design_doc_input(project_id: &str, path: &str) -> SetProjectDesignDocInput {
        SetProjectDesignDocInput {
            project_id: project_id.to_owned(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: Some(path.to_owned()),
            unset: false,
        }
    }

    #[test]
    fn set_project_design_doc_rejects_empty_path() {
        let path = temp_db_path("design-doc-empty-path");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .set_project_design_doc(set_design_doc_input(&project.id, "   "))
            .unwrap_err()
            .to_string();
        assert!(err.contains("may not be empty"), "got: {err}");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_rejects_absolute_path() {
        let path = temp_db_path("design-doc-abs-path");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .set_project_design_doc(set_design_doc_input(&project.id, "/etc/passwd.md"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("repo-relative"), "got: {err}");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_rejects_dotdot_segments() {
        let path = temp_db_path("design-doc-dotdot");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .set_project_design_doc(set_design_doc_input(
                &project.id,
                "tools/../../../etc/passwd.md",
            ))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`..`"), "got: {err}");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_rejects_cube_workspace_path() {
        // A workspace-relative path like
        // `cube/workspaces/<id>/tools/boss/docs/designs/foo.md` starts
        // without `/` so the absolute-path guard misses it. The explicit
        // `cube/workspaces/` check catches it.
        let path = temp_db_path("design-doc-cube-ws-path");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .set_project_design_doc(set_design_doc_input(
                &project.id,
                "cube/workspaces/mono-agent-001/tools/boss/docs/designs/foo.md",
            ))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cube/workspaces/"),
            "expected cube/workspaces guard error, got: {err}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_rejects_bad_extension() {
        let path = temp_db_path("design-doc-bad-ext");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .set_project_design_doc(set_design_doc_input(
                &project.id,
                "tools/boss/docs/designs/foo.html",
            ))
            .unwrap_err()
            .to_string();
        assert!(err.contains("markdown"), "got: {err}");

        // And `.md` / `.markdown` both pass.
        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/foo.md",
        ))
        .unwrap();
        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/foo.markdown",
        ))
        .unwrap();

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_unset_clears_all_three_columns() {
        let path = temp_db_path("design-doc-unset");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(
                "https://github.com/myorg/wiki.git".to_owned(),
            ),
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();
        let after_set = db.get_project(&project.id).unwrap();
        assert_eq!(
            after_set.design_doc_repo_remote_url.as_deref(),
            Some("https://github.com/myorg/wiki.git"),
        );
        assert_eq!(after_set.design_doc_branch.as_deref(), Some("docs"));
        assert_eq!(after_set.design_doc_path.as_deref(), Some("designs/foo.md"));

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: None,
            unset: true,
        })
        .unwrap();

        let cleared = db.get_project(&project.id).unwrap();
        assert!(cleared.design_doc_repo_remote_url.is_none());
        assert!(cleared.design_doc_branch.is_none());
        assert!(cleared.design_doc_path.is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_canonicalises_repo_url() {
        let path = temp_db_path("design-doc-canonical-repo");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        // Surrounding whitespace and a blank branch should normalise
        // away the same way `products.repo_remote_url` does.
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(
                "  https://github.com/myorg/wiki.git  ".to_owned(),
            ),
            design_doc_branch: Some("   ".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        let stored = db.get_project(&project.id).unwrap();
        assert_eq!(
            stored.design_doc_repo_remote_url.as_deref(),
            Some("https://github.com/myorg/wiki.git"),
        );
        assert!(stored.design_doc_branch.is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_is_last_writer_wins() {
        let path = temp_db_path("design-doc-last-writer");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/first.md",
        ))
        .unwrap();
        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/second.md",
        ))
        .unwrap();
        let after_set = db.get_project(&project.id).unwrap();
        assert_eq!(
            after_set.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/second.md"),
        );

        // Then unset.
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: None,
            unset: true,
        })
        .unwrap();
        assert!(
            db.get_project(&project.id)
                .unwrap()
                .design_doc_path
                .is_none()
        );

        // Then set again — clears the cleared state, no residue.
        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/third.md",
        ))
        .unwrap();
        assert_eq!(
            db.get_project(&project.id)
                .unwrap()
                .design_doc_path
                .as_deref(),
            Some("tools/boss/docs/designs/third.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_project_design_doc_with_no_path_only_updates_overrides() {
        let path = temp_db_path("design-doc-overrides-only");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: Some("tools/boss/docs/designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        // Now patch only the branch; the path must stay put.
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: None,
            unset: false,
        })
        .unwrap();
        let stored = db.get_project(&project.id).unwrap();
        assert_eq!(stored.design_doc_branch.as_deref(), Some("docs"));
        assert_eq!(
            stored.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/foo.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_project_design_doc_returns_not_set_when_path_null() {
        let path = temp_db_path("resolve-not-set");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| None)
            .unwrap();
        assert_eq!(resolved.project_id, project.id);
        assert!(matches!(resolved.state, ProjectDesignDocState::NotSet));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_project_design_doc_same_product_inherits_repo_and_default_branch() {
        let path = temp_db_path("resolve-same-product");
        let db = WorkDb::open(path.clone()).unwrap();
        let (product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/foo.md",
        ))
        .unwrap();

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| {
                Some("/tmp/mono-agent-007".to_owned())
            })
            .unwrap();
        let ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            raw_content_url,
        } = resolved.state
        else {
            panic!("expected Resolved state, got {:?}", resolved.state);
        };
        assert_eq!(resolved.repo_remote_url, "git@github.com:spinyfin/mono.git");
        assert_eq!(resolved.branch, "main");
        assert_eq!(resolved.path, "tools/boss/docs/designs/foo.md");
        assert_eq!(
            resolved.kind,
            ResolvedDesignDocKind::SameProduct {
                product_id: product.id.clone(),
            }
        );
        assert_eq!(workspace_path.as_deref(), Some("/tmp/mono-agent-007"));
        // Repo URL is `git@github.com:spinyfin/mono.git` → web URL
        // renders against the parsed `spinyfin/mono` slug.
        assert_eq!(
            web_url,
            "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/designs/foo.md",
        );
        assert_eq!(
            raw_content_url.as_deref(),
            Some("https://raw.githubusercontent.com/spinyfin/mono/main/tools/boss/docs/designs/foo.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_project_design_doc_classifies_other_product() {
        let path = temp_db_path("resolve-other-product");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        // A second Boss product whose repo owns the design doc.
        let wiki_product = db
            .create_product(CreateProductInput {
                name: "Wiki".to_owned(),
                description: None,
                repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
            })
            .unwrap();

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(
                "https://github.com/myorg/wiki.git".to_owned(),
            ),
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| None)
            .unwrap();
        let ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            raw_content_url,
        } = resolved.state
        else {
            panic!("expected Resolved state");
        };
        assert_eq!(resolved.branch, "docs");
        assert_eq!(
            resolved.kind,
            ResolvedDesignDocKind::OtherProduct {
                product_id: wiki_product.id,
            }
        );
        assert!(workspace_path.is_none());
        assert_eq!(
            web_url,
            "https://github.com/myorg/wiki/blob/docs/designs/foo.md",
        );
        assert_eq!(
            raw_content_url.as_deref(),
            Some("https://raw.githubusercontent.com/myorg/wiki/docs/designs/foo.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_project_design_doc_classifies_external() {
        let path = temp_db_path("resolve-external");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(
                "https://github.com/external/other.git".to_owned(),
            ),
            design_doc_branch: None,
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| None)
            .unwrap();
        let ProjectDesignDocState::Resolved { resolved, .. } = resolved.state else {
            panic!("expected Resolved state");
        };
        assert_eq!(resolved.kind, ResolvedDesignDocKind::External);

        let _ = std::fs::remove_file(path);
    }

    /// Regression for Bug A: SSH-form remote URLs (`git@github.com:owner/repo.git`)
    /// must produce a non-null `raw_content_url`. The resolver inherits the
    /// product's `repo_remote_url` when `design_doc_repo_remote_url` is unset;
    /// if that URL is in SCP/SSH form the raw URL builder must still work.
    /// The test uses a non-main `design_doc_branch` to simulate an in-review
    /// design PR — the branch must appear in the raw URL, not "main".
    #[test]
    fn resolve_project_design_doc_raw_content_url_built_for_ssh_remote_on_pr_branch() {
        let path = temp_db_path("resolve-raw-content-ssh-pr-branch");
        let db = WorkDb::open(path.clone()).unwrap();
        // seed_project_for_design_doc uses `git@github.com:spinyfin/mono.git` (SSH form).
        let (_, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None, // inherit SSH URL from product
            design_doc_branch: Some("design-boss-ci-buildkite".to_owned()),
            design_doc_path: Some("tools/boss/docs/designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| None)
            .unwrap();
        let ProjectDesignDocState::Resolved { raw_content_url, web_url, .. } = resolved.state
        else {
            panic!("expected Resolved, got {:?}", resolved.state);
        };

        assert_eq!(
            raw_content_url.as_deref(),
            Some("https://raw.githubusercontent.com/spinyfin/mono/design-boss-ci-buildkite/tools/boss/docs/designs/foo.md"),
            "SSH remote URL must produce a raw_content_url on a non-main branch"
        );
        assert_eq!(
            web_url,
            "https://github.com/spinyfin/mono/blob/design-boss-ci-buildkite/tools/boss/docs/designs/foo.md",
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_project_design_doc_surfaces_broken_when_no_repo() {
        let path = temp_db_path("resolve-broken");
        let db = WorkDb::open(path.clone()).unwrap();

        // Product without a repo_remote_url, so a project that
        // doesn't supply one either has nothing to resolve against.
        let product = db
            .create_product(CreateProductInput {
                name: "NoRepo".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Broken".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "designs/foo.md",
        ))
        .unwrap();

        let resolved = db
            .resolve_project_design_doc(&project.id, |_| None)
            .unwrap();
        match resolved.state {
            ProjectDesignDocState::Broken { reason } => {
                assert!(
                    reason.contains("repo"),
                    "broken reason should mention the missing repo: {reason}"
                );
            }
            other => panic!("expected Broken state, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sync_project_design_doc_from_detector_populates_when_null() {
        let path = temp_db_path("detector-sync-empty");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let wrote = db
            .sync_project_design_doc_from_detector(
                &project.id,
                Some("git@github.com:spinyfin/mono.git"),
                Some("main"),
                "tools/boss/docs/designs/foo.md",
            )
            .unwrap();
        assert!(wrote, "expected the detector hook to write");

        let updated = db.get_project(&project.id).unwrap();
        assert_eq!(
            updated.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/foo.md"),
        );
        assert_eq!(
            updated.design_doc_repo_remote_url.as_deref(),
            Some("git@github.com:spinyfin/mono.git"),
        );
        assert_eq!(updated.design_doc_branch.as_deref(), Some("main"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sync_project_design_doc_from_detector_skips_when_pointer_set() {
        let path = temp_db_path("detector-sync-skip");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/manual.md",
        ))
        .unwrap();

        let wrote = db
            .sync_project_design_doc_from_detector(
                &project.id,
                Some("git@github.com:spinyfin/mono.git"),
                Some("main"),
                "tools/boss/docs/designs/from-detector.md",
            )
            .unwrap();
        assert!(!wrote, "expected the detector hook to no-op");

        let unchanged = db.get_project(&project.id).unwrap();
        assert_eq!(
            unchanged.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/manual.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sync_project_design_doc_from_detector_validates_path() {
        let path = temp_db_path("detector-sync-bad-path");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let err = db
            .sync_project_design_doc_from_detector(
                &project.id,
                None,
                None,
                "/absolute/path.md",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("repo-relative"), "got: {err}");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_records_first_set_as_old_null_new_value() {
        let path = temp_db_path("audit-first-set");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/foo.md",
        ))
        .unwrap();

        let audit = db.list_project_property_audit(&project.id).unwrap();
        assert_eq!(
            audit.len(),
            1,
            "path-only edit on a fresh project should produce exactly one row, got {audit:#?}",
        );
        assert_eq!(audit[0].property, "design_doc_path");
        assert!(audit[0].old_value.is_none());
        assert_eq!(
            audit[0].new_value.as_deref(),
            Some("tools/boss/docs/designs/foo.md"),
        );
        assert_eq!(audit[0].actor, AUDIT_ACTOR_HUMAN);
        assert_eq!(audit[0].project_id, project.id);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_records_one_row_per_changed_column() {
        let path = temp_db_path("audit-three-cols");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();

        let audit = db.list_project_property_audit(&project.id).unwrap();
        let properties: HashSet<&str> = audit.iter().map(|e| e.property.as_str()).collect();
        assert_eq!(properties.len(), 3, "got: {audit:#?}");
        assert!(properties.contains("design_doc_repo_remote_url"));
        assert!(properties.contains("design_doc_branch"));
        assert!(properties.contains("design_doc_path"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_no_op_writes_emit_no_extra_rows() {
        let path = temp_db_path("audit-noop");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let input = SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        };
        db.set_project_design_doc(input.clone()).unwrap();
        let after_first = db.list_project_property_audit(&project.id).unwrap().len();
        db.set_project_design_doc(input).unwrap();
        let after_second = db.list_project_property_audit(&project.id).unwrap().len();
        assert_eq!(
            after_first, after_second,
            "second identical write should not emit any audit rows",
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_records_unset_as_old_value_new_null() {
        let path = temp_db_path("audit-unset");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
            design_doc_branch: Some("docs".to_owned()),
            design_doc_path: Some("designs/foo.md".to_owned()),
            unset: false,
        })
        .unwrap();
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: None,
            unset: true,
        })
        .unwrap();

        let audit = db.list_project_property_audit(&project.id).unwrap();
        assert_eq!(
            audit.len(),
            6,
            "3 set + 3 unset = 6 rows, got: {audit:#?}",
        );
        for entry in &audit[3..] {
            assert!(
                entry.old_value.is_some(),
                "unset row should retain the prior value as old_value",
            );
            assert!(
                entry.new_value.is_none(),
                "unset row should record new_value as NULL",
            );
            assert_eq!(entry.actor, AUDIT_ACTOR_HUMAN);
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_records_detector_actor_on_sync() {
        let path = temp_db_path("audit-detector-actor");
        let db = WorkDb::open(path.clone()).unwrap();
        let (_product, project) = seed_project_for_design_doc(&db);

        let wrote = db
            .sync_project_design_doc_from_detector(
                &project.id,
                Some("git@github.com:spinyfin/mono.git"),
                Some("main"),
                "tools/boss/docs/designs/foo.md",
            )
            .unwrap();
        assert!(wrote);

        let audit = db.list_project_property_audit(&project.id).unwrap();
        assert!(
            !audit.is_empty(),
            "detector sync should emit at least one audit row",
        );
        for entry in &audit {
            assert_eq!(
                entry.actor, AUDIT_ACTOR_DESIGN_DETECTOR,
                "detector-sync rows must carry the engine actor: {entry:#?}",
            );
        }
        let property_set: HashSet<&str> = audit.iter().map(|e| e.property.as_str()).collect();
        assert!(property_set.contains("design_doc_path"));

        let _ = std::fs::remove_file(path);
    }

    /// Helper: stand up an execution attached to the given project so
    /// the conflict-surfacing test has a foreign key it can attach
    /// the attention item to.
    fn seed_execution_for(db: &WorkDb, product_id: &str, project_id: &str) -> WorkExecution {
        let task = db
            .create_task(CreateTaskInput {
                product_id: product_id.to_owned(),
                project_id: project_id.to_owned(),
                name: "Schema".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: task.id,
            kind: "task_implementation".to_owned(),
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
        .unwrap()
    }

    #[test]
    fn surface_design_doc_conflict_on_approve_no_pointer_is_no_op() {
        let path = temp_db_path("approve-conflict-no-pointer");
        let db = WorkDb::open(path.clone()).unwrap();
        let (product, project) = seed_project_for_design_doc(&db);
        let execution = seed_execution_for(&db, &product.id, &project.id);

        let item = db
            .surface_design_doc_conflict_on_approve(
                &project.id,
                &execution.id,
                None,
                None,
                "tools/boss/docs/designs/foo.md",
            )
            .unwrap();
        assert!(item.is_none());
        assert!(db.list_attention_items(&execution.id).unwrap().is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn surface_design_doc_conflict_on_approve_silent_when_pointer_matches() {
        let path = temp_db_path("approve-conflict-match");
        let db = WorkDb::open(path.clone()).unwrap();
        let (product, project) = seed_project_for_design_doc(&db);
        let execution = seed_execution_for(&db, &product.id, &project.id);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/foo.md",
        ))
        .unwrap();

        // Approved doc matches: same path, inherits same repo, default
        // branch matches the resolved default.
        let item = db
            .surface_design_doc_conflict_on_approve(
                &project.id,
                &execution.id,
                None,
                None,
                "tools/boss/docs/designs/foo.md",
            )
            .unwrap();
        assert!(item.is_none(), "expected silent no-op when pointers agree");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn surface_design_doc_conflict_on_approve_emits_attention_item_when_pointer_differs() {
        let path = temp_db_path("approve-conflict-emits");
        let db = WorkDb::open(path.clone()).unwrap();
        let (product, project) = seed_project_for_design_doc(&db);
        let execution = seed_execution_for(&db, &product.id, &project.id);

        db.set_project_design_doc(set_design_doc_input(
            &project.id,
            "tools/boss/docs/designs/manual.md",
        ))
        .unwrap();

        let item = db
            .surface_design_doc_conflict_on_approve(
                &project.id,
                &execution.id,
                None,
                None,
                "tools/boss/docs/designs/from-task.md",
            )
            .unwrap()
            .expect("conflict should surface an attention item");
        assert_eq!(item.kind, "design_doc_pointer_conflict");
        assert!(
            item.body_markdown.contains("manual.md"),
            "body should name the project's path: {}",
            item.body_markdown,
        );
        assert!(
            item.body_markdown.contains("from-task.md"),
            "body should name the approved path: {}",
            item.body_markdown,
        );

        let items = db.list_attention_items(&execution.id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "design_doc_pointer_conflict");

        // Project pointer must not be overwritten by the helper.
        let unchanged = db.get_project(&project.id).unwrap();
        assert_eq!(
            unchanged.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/manual.md"),
        );

        let _ = std::fs::remove_file(path);
    }

    /// Regression: previously, four `boss chore bind-pr` calls in
    /// flight against the same engine would race on a single sqlite
    /// connection-per-call with no busy-timeout, and one of them
    /// would surface "database is locked" to the caller. With WAL +
    /// busy_timeout + IMMEDIATE transactions, concurrent writes on
    /// distinct rows must all succeed.
    #[test]
    fn concurrent_writes_do_not_return_database_locked() {
        const WORKERS: usize = 8;

        // Must use an on-disk database: WAL mode (which serialises
        // concurrent writers via busy_timeout) is incompatible with
        // SQLite's shared-cache in-memory mode, causing
        // SQLITE_LOCKED_SHAREDCACHE errors that busy_timeout cannot retry.
        let path = disk_db_path("concurrent-writes");
        let db = std::sync::Arc::new(WorkDb::open(path.clone()).unwrap());

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();

        // One chore per worker, so each write hits a distinct row —
        // matching the real-world reconcile pattern where a script
        // binds N PRs to N different chores in parallel.
        let chore_ids: Vec<String> = (0..WORKERS)
            .map(|i| {
                db.create_chore(CreateChoreInput {
                    product_id: product.id.clone(),
                    name: format!("Concurrent chore {i}"),
                    description: None,
                    autostart: false,
                    priority: None,
                    created_via: None,
                    repo_remote_url: None,
                    effort_level: None,
                    model_override: None,
                    force_duplicate: false,
                })
                .unwrap()
                .id
            })
            .collect();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
        let handles: Vec<_> = chore_ids
            .iter()
            .enumerate()
            .map(|(i, chore_id)| {
                let db = db.clone();
                let chore_id = chore_id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    // Park every worker at the gate so the writes
                    // truly land on the engine at the same instant.
                    barrier.wait();
                    db.update_work_item(
                        &chore_id,
                        WorkItemPatch {
                            pr_url: Some(format!(
                                "https://github.com/spinyfin/mono/pull/{}",
                                100 + i
                            )),
                            ..WorkItemPatch::default()
                        },
                    )
                })
            })
            .collect();

        let mut failures: Vec<String> = Vec::new();
        for (i, handle) in handles.into_iter().enumerate() {
            match handle.join().unwrap() {
                Ok(_) => {}
                Err(err) => failures.push(format!("worker {i}: {err:#}")),
            }
        }
        assert!(
            failures.is_empty(),
            "expected all {WORKERS} concurrent writes to succeed, got failures: {failures:?}",
        );

        // And the writes must have actually persisted, not silently
        // been swallowed by a retry that lost its update.
        for (i, chore_id) in chore_ids.iter().enumerate() {
            let item = db.get_work_item(chore_id).unwrap();
            let WorkItem::Chore(task) = item else {
                panic!("expected chore {chore_id} to round-trip as a Chore");
            };
            assert_eq!(
                task.pr_url.as_deref(),
                Some(format!("https://github.com/spinyfin/mono/pull/{}", 100 + i).as_str()),
            );
        }

        let _ = std::fs::remove_file(&path);
        // WAL writes leave -wal / -shm sidecar files; clean them up
        // so the temp dir doesn't leak.
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// Shared scaffold for the `resolve_repo_for_work_item` tests: a
    /// product (with `product_repo`) carrying a project + one task
    /// whose own `repo_remote_url` is left `NULL`. Tests plant the
    /// override they want via `set_task_repo` and then exercise the
    /// helper.
    fn make_resolve_scaffold(
        label: &str,
        product_repo: Option<&str>,
    ) -> (PathBuf, WorkDb, String, String) {
        // disk_db_path so that resolve_repo_errors_when_parent_product_is_missing
        // can open a second raw connection to the same database file.
        let path = disk_db_path(label);
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: product_repo.map(str::to_owned),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Project".to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        // When the product has no repo, `create_task` now rejects a
        // None override (multi-repo products require a row-level repo).
        // These resolver tests need to probe pre-existing legacy rows
        // that have both task and product repo = NULL, so we bypass the
        // creation-time validation and insert directly via SQL.
        let task_id = if product_repo.is_none() {
            let conn = db.connect().unwrap();
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, ?3, 'project_task', 'Task', '', 'todo', 1, NULL, NULL, ?4, ?4, 0, 'medium', 'test')",
                params![id, product.id, project.id, now],
            ).unwrap();
            id
        } else {
            db.create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Task".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap()
            .id
        };
        (path, db, product.id, task_id)
    }

    /// Resolver tests plant the override directly via SQL so they can
    /// probe arbitrary combinations (including legacy rows that violate
    /// the new invariant). Using `db.connect()` keeps the WAL /
    /// busy-timeout pragmas consistent with the helper's read path.
    fn set_task_repo(db: &WorkDb, task_id: &str, value: Option<&str>) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
            params![task_id, value],
        )
        .unwrap();
    }

    #[test]
    fn resolve_repo_returns_task_override_when_set() {
        let (_path, db, _product_id, task_id) = make_resolve_scaffold(
            "resolve-override-set",
            Some("git@github.com:spinyfin/product-default.git"),
        );
        set_task_repo(&db, &task_id, Some("git@github.com:spinyfin/per-task.git"));

        let conn = db.connect().unwrap();
        let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("git@github.com:spinyfin/per-task.git"),
            "non-empty task override must win over the product default",
        );
    }

    #[test]
    fn resolve_repo_treats_empty_override_as_unset() {
        let (_path, db, _product_id, task_id) = make_resolve_scaffold(
            "resolve-override-empty",
            Some("git@github.com:spinyfin/product-default.git"),
        );
        set_task_repo(&db, &task_id, Some(""));

        let conn = db.connect().unwrap();
        let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("git@github.com:spinyfin/product-default.git"),
            "empty-string override must fall through to the product default",
        );
    }

    #[test]
    fn resolve_repo_falls_back_to_product_when_override_null() {
        let (_path, db, _product_id, task_id) = make_resolve_scaffold(
            "resolve-override-null",
            Some("git@github.com:spinyfin/product-default.git"),
        );
        // Leave tasks.repo_remote_url at its insert-time NULL.

        let conn = db.connect().unwrap();
        let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("git@github.com:spinyfin/product-default.git"),
            "NULL override must inherit from the product",
        );
    }

    #[test]
    fn resolve_repo_returns_none_when_both_null() {
        let (_path, db, _product_id, task_id) =
            make_resolve_scaffold("resolve-both-null", None);
        // Both tasks.repo_remote_url and products.repo_remote_url are
        // NULL; the dispatcher will treat the Ok(None) as an
        // unresolved row and record an attention item.

        let conn = db.connect().unwrap();
        let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
        assert!(
            resolved.is_none(),
            "both-NULL must resolve to Ok(None), got {resolved:?}",
        );
    }

    #[test]
    fn resolve_repo_errors_when_parent_product_is_missing() {
        let (path, db, product_id, task_id) = make_resolve_scaffold(
            "resolve-orphan-product",
            Some("git@github.com:spinyfin/product-default.git"),
        );

        // Drop the parent product behind FK enforcement so the task
        // is left pointing at a non-existent product_id — the
        // referential-integrity break the helper must surface.
        let raw = Connection::open(&path).unwrap();
        // PRAGMA foreign_keys defaults to OFF on a fresh connection,
        // but state it explicitly so the test reads correctly.
        raw.pragma_update(None, "foreign_keys", false).unwrap();
        raw.execute("DELETE FROM products WHERE id = ?1", [&product_id])
            .unwrap();
        drop(raw);

        let conn = db.connect().unwrap();
        let err = resolve_repo_for_work_item(&conn, &task_id).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("orphan task") && message.contains(&task_id),
            "expected an orphan-task error mentioning the task id, got: {message}",
        );
    }

    /// Default-shape sanity: a freshly-created chore/task has NULL
    /// for the new effort/model columns; a freshly-created product
    /// has NULL for `default_model`. Confirms the migration's
    /// "behaviour unchanged for unset rows" contract holds on a
    /// brand-new DB (the easy case — the migration test below
    /// covers an upgrade-in-place).
    #[test]
    fn effort_and_model_default_to_null_on_fresh_rows() {
        let path = temp_db_path("effort-fresh");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        assert!(product.default_model.is_none());
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Trivial fix".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert!(chore.effort_level.is_none());
        assert!(chore.model_override.is_none());
        let _ = std::fs::remove_file(path);
    }

    /// `create_chore` with `effort_level` / `model_override` set
    /// writes both columns; `query_task` reads them back through
    /// `map_task` faithfully.
    #[test]
    fn effort_and_model_roundtrip_through_create_and_query() {
        let path = temp_db_path("effort-roundtrip");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Big investigation".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: Some(EffortLevel::Large),
                model_override: Some("claude-opus-4-7".into()),
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(chore.effort_level, Some(EffortLevel::Large));
        assert_eq!(chore.model_override.as_deref(), Some("claude-opus-4-7"));
        let _ = std::fs::remove_file(path);
    }

    /// Update verb honours `--effort` set/clear and `--model`
    /// set/clear semantics (empty string clears, anything else
    /// stores verbatim).
    #[test]
    fn update_chore_sets_and_clears_effort_and_model() {
        let path = temp_db_path("effort-update");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Some work".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        // Set via update.
        let updated = db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    effort_level: Some("medium".into()),
                    model_override: Some("sonnet".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        let task = match updated {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            _ => panic!("expected chore/task item"),
        };
        assert_eq!(task.effort_level, Some(EffortLevel::Medium));
        assert_eq!(task.model_override.as_deref(), Some("sonnet"));

        // Clear via empty string.
        let cleared = db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    effort_level: Some(String::new()),
                    model_override: Some(String::new()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        let task = match cleared {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            _ => panic!("expected chore/task item"),
        };
        assert!(task.effort_level.is_none());
        assert!(task.model_override.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// Update verb rejects an invalid `effort_level` string with a
    /// clear error that names the allowed values.
    #[test]
    fn update_chore_rejects_invalid_effort_level() {
        let path = temp_db_path("effort-invalid");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Some work".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        let err = db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    effort_level: Some("galaxybrain".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("galaxybrain"));
        assert!(message.contains("trivial"));
        assert!(message.contains("max"));

        // Row was not partially updated — effort_level remains NULL.
        let after = db
            .update_work_item(
                &chore.id,
                WorkItemPatch {
                    name: Some("force a no-op write so we can re-read".into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        let task = match after {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            _ => panic!("expected chore/task item"),
        };
        assert!(task.effort_level.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// `set_product_default_model` round-trip: set then clear.
    /// Slugs are stored verbatim (no validation).
    #[test]
    fn product_default_model_set_and_clear() {
        let path = temp_db_path("default-model");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        assert!(product.default_model.is_none());

        let with_model = db
            .set_product_default_model(&product.id, Some("sonnet"))
            .unwrap();
        assert_eq!(with_model.default_model.as_deref(), Some("sonnet"));

        // Verbatim — engine does not normalise the slug.
        let verbatim = db
            .set_product_default_model(&product.id, Some("an-unreleased-model-2099"))
            .unwrap();
        assert_eq!(
            verbatim.default_model.as_deref(),
            Some("an-unreleased-model-2099"),
        );

        let cleared = db
            .set_product_default_model(&product.id, Some(""))
            .unwrap();
        assert!(cleared.default_model.is_none());

        let cleared_again = db
            .set_product_default_model(&product.id, None)
            .unwrap();
        assert!(cleared_again.default_model.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// Drop the effort/model columns (simulating a pre-PR-370 DB)
    /// and re-open: the migration's ALTER TABLE path must re-add
    /// them and leave existing rows with NULL on each new column.
    /// SQLite 3.35+ supports `ALTER TABLE … DROP COLUMN`, which lets
    /// us replay an upgrade-in-place without hand-rolling the
    /// pre-v7 schema from scratch.
    #[test]
    fn migration_re_adds_effort_and_model_columns_on_upgrade() {
        // disk_db_path required: drops columns and re-opens the DB to trigger migration.
        let path = disk_db_path("effort-upgrade");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Legacy chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        {
            let conn = db.connect().unwrap();
            // Drop the new columns to simulate a pre-migration DB.
            conn.execute("ALTER TABLE tasks DROP COLUMN effort_level", [])
                .unwrap();
            conn.execute("ALTER TABLE tasks DROP COLUMN model_override", [])
                .unwrap();
            conn.execute("ALTER TABLE products DROP COLUMN default_model", [])
                .unwrap();
            assert!(!table_has_column(&conn, "tasks", "effort_level").unwrap());
            assert!(!table_has_column(&conn, "tasks", "model_override").unwrap());
            assert!(!table_has_column(&conn, "products", "default_model").unwrap());
        }
        drop(db);

        // Re-open re-runs the migrations.
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "effort_level").unwrap());
        assert!(table_has_column(&conn, "tasks", "model_override").unwrap());
        assert!(table_has_column(&conn, "products", "default_model").unwrap());

        let chore_effort: Option<String> = conn
            .query_row(
                "SELECT effort_level FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap();
        let chore_model: Option<String> = conn
            .query_row(
                "SELECT model_override FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap();
        let product_model: Option<String> = conn
            .query_row(
                "SELECT default_model FROM products WHERE id = ?1",
                [&product.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(chore_effort.is_none());
        assert!(chore_model.is_none());
        assert!(product_model.is_none());

        // Post-migration rows can carry any of the five enum
        // values; the round-trip continues to work.
        let after_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Post-migration chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: Some(EffortLevel::Trivial),
                model_override: Some("haiku".into()),
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(after_chore.effort_level, Some(EffortLevel::Trivial));
        assert_eq!(after_chore.model_override.as_deref(), Some("haiku"));

        let _ = std::fs::remove_file(path);
    }

    /// Migration test: rows created against a pre-migration schema
    /// keep `NULL` for the new columns after the migration runs.
    /// Mirrors the legacy-row contract every prior migration is
    /// expected to honour.
    #[test]
    fn migration_leaves_existing_rows_with_null_effort_and_model() {
        // disk_db_path required: re-opens the DB to trigger migration.
        let path = disk_db_path("effort-migrate");

        // Stand up a "pre-migration" DB by hand-rolling rows with the
        // older column set, then re-open via `WorkDb::open` so the
        // migration runs against it. We don't replay the entire pre-v7
        // schema; we just drop the new columns on a freshly-init'd DB
        // to simulate the upgrade path.
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:test/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Pre-migration chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // Simulate the pre-migration state by NULL-ing whatever the
        // current schema initialised. `create_chore` already stores
        // NULL for `effort_level` / `model_override`, and
        // `create_product` already stores NULL for `default_model`,
        // so we just confirm that — the explicit ALTER-TABLE path on
        // re-open is exercised by the legacy-on-disk DBs in the
        // field, which the upgrade test below would otherwise be a
        // synthetic re-init of.
        drop(db);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        let chore_effort: Option<String> = conn
            .query_row(
                "SELECT effort_level FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap();
        let chore_model: Option<String> = conn
            .query_row(
                "SELECT model_override FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap();
        let product_model: Option<String> = conn
            .query_row(
                "SELECT default_model FROM products WHERE id = ?1",
                [&product.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(chore_effort.is_none());
        assert!(chore_model.is_none());
        assert!(product_model.is_none());

        let _ = std::fs::remove_file(path);
    }

    /// Cleanup migration: rows where `tasks.repo_remote_url` mirrors the
    /// parent product's repo get set to NULL; rows with a genuinely
    /// divergent override (legitimate multi-repo task overrides) are
    /// left unchanged.
    #[test]
    fn migrate_null_redundant_task_repo_remote_urls_clears_mirrors_and_preserves_divergent() {
        // disk_db_path required: the test re-opens the DB to trigger the migration.
        let path = disk_db_path("migration-null-redundant-repos");
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();

        // Product with repo_remote_url = "git@example.com:foo.git".
        let product = db.create_product(CreateProductInput {
            name: "Foo".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:foo.git".to_owned()),
        }).unwrap();
        let project = db.create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Proj".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        }).unwrap();

        // Seed 3 chores that mirror the product's repo (the legacy bug).
        // We bypass the API to plant the now-invalid state directly.
        let mirrored_ids: Vec<String> = (0..3).map(|i| {
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
                 VALUES (?1, ?2, NULL, 'chore', ?3, '', 'todo', NULL, NULL, NULL, ?4, ?4, 0, 'medium', 'test', 'git@example.com:foo.git')",
                params![id, product.id, format!("chore-mirror-{i}"), now],
            ).unwrap();
            id
        }).collect();

        // Seed 1 chore with a legitimately different repo (multi-repo override).
        let divergent_id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
             VALUES (?1, ?2, NULL, 'chore', 'divergent', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', 'git@example.com:other.git')",
            params![divergent_id, product.id, now],
        ).unwrap();

        // Also seed a task (with project_id) that mirrors the product's repo.
        let mirrored_task_id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
             VALUES (?1, ?2, ?3, 'project_task', 'mirrored-task', '', 'todo', 5, NULL, NULL, ?4, ?4, 0, 'medium', 'test', 'git@example.com:foo.git')",
            params![mirrored_task_id, product.id, project.id, now],
        ).unwrap();

        // Re-open the DB to trigger the migration.
        drop(conn);
        let db2 = WorkDb::open(path.clone()).unwrap();
        let conn2 = db2.connect().unwrap();

        // All mirrored rows must now have repo_remote_url = NULL.
        for id in &mirrored_ids {
            let val: Option<String> = conn2.query_row(
                "SELECT repo_remote_url FROM tasks WHERE id = ?1",
                [id],
                |row| row.get(0),
            ).unwrap();
            assert!(val.is_none(), "mirrored chore {id} must be NULL after migration, got {val:?}");
        }
        let mirrored_task_val: Option<String> = conn2.query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&mirrored_task_id],
            |row| row.get(0),
        ).unwrap();
        assert!(mirrored_task_val.is_none(), "mirrored task must be NULL after migration, got {mirrored_task_val:?}");

        // The divergent override must remain unchanged.
        let divergent_val: Option<String> = conn2.query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&divergent_id],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(
            divergent_val.as_deref(),
            Some("git@example.com:other.git"),
            "divergent override must survive migration unchanged",
        );

        drop(conn2);
        let _ = std::fs::remove_file(path);
    }

    /// Two threads creating chores against the same `WorkDb` (and the
    /// same product) must each get a distinct `short_id`. The
    /// allocator is wrapped in SQLite's per-write serialisation
    /// (`BEGIN IMMEDIATE` + `busy_timeout`), so the test asserts the
    /// emergent property: N parallel inserts produce N distinct,
    /// gap-free ids starting at 1.
    #[test]
    fn allocator_concurrent_inserts_produce_distinct_short_ids() {
        // Must use an on-disk database: WAL mode (which serialises
        // concurrent writers via busy_timeout) is incompatible with
        // SQLite's shared-cache in-memory mode.
        let path = disk_db_path("short-id-concurrent");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@example.com:concurrent.git".into()),
            })
            .unwrap();

        const N: usize = 16;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let db = db.clone();
            let product_id = product.id.clone();
            handles.push(std::thread::spawn(move || {
                db.create_chore(CreateChoreInput {
                    product_id,
                    name: format!("c{i}"),
                    description: None,
                    autostart: false,
                    priority: None,
                    created_via: None,
                    repo_remote_url: None,
                    effort_level: None,
                    model_override: None,
                    force_duplicate: false,
                })
                .unwrap()
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let conn = db.connect().unwrap();
        // Collect every `short_id` for this product across both tables
        // (the per-product sequence is shared by `tasks` and
        // `projects`; the single `projects` row from `create_product`
        // doesn't create one, but the product itself does not — only
        // the design task for a project would, and we created no
        // project here). The N chores should occupy a contiguous run
        // starting at 1.
        let mut ids: Vec<i64> = conn
            .prepare(
                "SELECT short_id FROM tasks WHERE product_id = ?1 AND short_id IS NOT NULL",
            )
            .unwrap()
            .query_map([&product.id], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.sort();
        assert_eq!(ids, (1..=N as i64).collect::<Vec<_>>(), "ids: {ids:?}");

        // The counter has advanced past every id we just observed.
        let next: i64 = conn
            .query_row(
                "SELECT next_value FROM short_id_sequences WHERE product_id = ?1",
                [&product.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(next, N as i64 + 1);

        drop(conn);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// Two products run independent sequences: each starts at 1, and
    /// the per-product counter increments only on inserts against
    /// that product.
    #[test]
    fn allocator_per_product_sequences_are_independent() {
        let path = temp_db_path("short-id-per-product");
        let db = WorkDb::open(path.clone()).unwrap();
        let boss = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".into()),
            })
            .unwrap();
        let flunge = db
            .create_product(CreateProductInput {
                name: "Flunge".into(),
                description: None,
                repo_remote_url: Some("git@example.com:flunge.git".into()),
            })
            .unwrap();

        let mk_chore = |product_id: &str, name: &str| {
            db.create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: name.to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap()
        };

        let b1 = mk_chore(&boss.id, "b1");
        let f1 = mk_chore(&flunge.id, "f1");
        let b2 = mk_chore(&boss.id, "b2");
        let f2 = mk_chore(&flunge.id, "f2");

        let conn = db.connect().unwrap();
        let short = |id: &str| -> i64 {
            conn.query_row(
                "SELECT short_id FROM tasks WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(short(&b1.id), 1);
        assert_eq!(short(&b2.id), 2);
        assert_eq!(short(&f1.id), 1);
        assert_eq!(short(&f2.id), 2);

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    /// Backfill is deterministic: given the same set of (product,
    /// created_at, id) tuples, two independent migration runs assign
    /// the same `short_id` to every row.
    ///
    /// Setup plants rows via raw SQL with NULL `short_id` and
    /// hand-controlled `created_at` values, then invokes the
    /// migration directly (the column / table already exist from
    /// `WorkDb::open`, but the rows are unnumbered). The merged
    /// `(created_at ASC, id ASC)` stream is the contract.
    #[test]
    fn migrate_short_id_backfill_is_deterministic_and_merges_tasks_and_projects() {
        fn seed_and_backfill(path: &Path) -> Vec<(String, i64)> {
            let db = WorkDb::open(path.to_path_buf()).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model)
                 VALUES ('prod_a', 'A', 'a', '', NULL, 'active', '0', '0', NULL)",
                [],
            )
            .unwrap();
            // Plant 3 tasks + 2 projects with hand-chosen created_at
            // values so the merged ordering is unambiguous. The
            // expected sequence by (created_at, id):
            //   100  task_a   -> 1
            //   100  task_b   -> 2  (created_at tie, id tiebreaker)
            //   200  proj_a   -> 3
            //   300  task_c   -> 4
            //   400  proj_b   -> 5
            let rows: &[(&str, &str, &str, i64)] = &[
                ("tasks",    "task_a", "chore", 100),
                ("tasks",    "task_b", "chore", 100),
                ("projects", "proj_a", "",     200),
                ("tasks",    "task_c", "chore", 300),
                ("projects", "proj_b", "",     400),
            ];
            for (table, id, kind, ts) in rows {
                if *table == "tasks" {
                    conn.execute(
                        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
                         VALUES (?1, 'prod_a', NULL, ?2, ?1, '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', NULL)",
                        params![id, kind, ts.to_string()],
                    )
                    .unwrap();
                } else {
                    conn.execute(
                        "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, short_id)
                         VALUES (?1, 'prod_a', ?1, ?1, '', '', 'planned', 'medium', ?2, ?2, NULL)",
                        params![id, ts.to_string()],
                    )
                    .unwrap();
                }
            }

            // Wipe the prior counter so the backfill replays from 1.
            conn.execute("DELETE FROM short_id_sequences WHERE product_id = 'prod_a'", []).unwrap();
            migrate_short_id_columns(&conn).unwrap();

            let mut pairs: Vec<(String, i64)> = Vec::new();
            for table in &["tasks", "projects"] {
                let sql = format!("SELECT id, short_id FROM {table} WHERE product_id = 'prod_a'");
                let mut stmt = conn.prepare(&sql).unwrap();
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })
                    .unwrap();
                for r in rows {
                    pairs.push(r.unwrap());
                }
            }
            pairs.sort();
            pairs
        }

        let path_a = temp_db_path("short-id-backfill-a");
        let path_b = temp_db_path("short-id-backfill-b");
        let run_a = seed_and_backfill(&path_a);
        let run_b = seed_and_backfill(&path_b);
        assert_eq!(run_a, run_b, "two independent runs must produce identical short_ids");

        let expected: Vec<(String, i64)> = vec![
            ("proj_a".into(), 3),
            ("proj_b".into(), 5),
            ("task_a".into(), 1),
            ("task_b".into(), 2),
            ("task_c".into(), 4),
        ];
        assert_eq!(run_a, expected);

        let _ = std::fs::remove_file(path_a);
        let _ = std::fs::remove_file(path_b);
    }

    /// The partial unique index `(product_id, short_id) WHERE
    /// short_id IS NOT NULL` is the belt-and-braces guard from design
    /// Q3 / Q8: a manual SQL insert that collides with an existing
    /// per-product `short_id` must be rejected.
    #[test]
    fn unique_short_id_index_rejects_manual_duplicate() {
        let path = temp_db_path("short-id-index-conflict");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "c1".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let existing_short: i64 = {
            let conn = db.connect().unwrap();
            conn.query_row(
                "SELECT short_id FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Try to hand-roll a second `tasks` row with the same
        // (product_id, short_id) — the partial unique index must
        // refuse it.
        let conn = db.connect().unwrap();
        let now = now_string();
        let manual_id = next_id("task");
        let err = conn
            .execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
                 VALUES (?1, ?2, NULL, 'chore', 'dupe', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', ?4)",
                params![manual_id, product.id, now, existing_short],
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("UNIQUE constraint failed"),
            "expected UNIQUE constraint failure, got: {err}",
        );

        // Same `short_id` on a DIFFERENT product is allowed — the
        // uniqueness invariant is `(product_id, short_id)`, not
        // global.
        let other = db
            .create_product(CreateProductInput {
                name: "Flunge".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let other_manual_id = next_id("task");
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
             VALUES (?1, ?2, NULL, 'chore', 'cross-product', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', ?4)",
            params![other_manual_id, other.id, now, existing_short],
        )
        .expect("same short_id on a different product must be permitted");

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    /// `create_project` allocates a `short_id` for the project row
    /// AND for the auto-spawned design task, both drawn from the
    /// per-product sequence (Q1: tasks and projects share a counter).
    #[test]
    fn create_project_assigns_short_ids_to_project_and_design_task() {
        let path = temp_db_path("short-id-project-design");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "P".into(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        let conn = db.connect().unwrap();
        let project_short: i64 = conn
            .query_row(
                "SELECT short_id FROM projects WHERE id = ?1",
                [&project.id],
                |row| row.get(0),
            )
            .unwrap();
        let design_short: i64 = conn
            .query_row(
                "SELECT short_id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
                [&project.id],
                |row| row.get(0),
            )
            .unwrap();
        // The project row is inserted before its design task, so it
        // gets the lower number.
        assert_eq!(project_short, 1);
        assert_eq!(design_short, 2);

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    /// `create_chore` returns a `Task` struct with `short_id` populated.
    /// This is the end-to-end wire test: the protocol struct carries the
    /// field through the full engine → protocol round-trip.
    #[test]
    fn create_chore_protocol_struct_carries_short_id() {
        let path = temp_db_path("short-id-wire-task");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
            })
            .unwrap();

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "wire-test".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(chore.short_id, Some(1), "first chore in product gets short_id 1");

        // A second chore in the same product gets the next number.
        let chore2 = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "wire-test-2".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        assert_eq!(chore2.short_id, Some(2));

        // `list_chores` also surfaces the field (exercises the SELECT path).
        let fetched = db.list_chores(&product.id, None).unwrap();
        assert_eq!(fetched[0].short_id, Some(1));

        let _ = std::fs::remove_file(path);
    }

    /// `create_project` returns a `Project` struct with `short_id` populated.
    #[test]
    fn create_project_protocol_struct_carries_short_id() {
        let path = temp_db_path("short-id-wire-project");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();

        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Wire Project".into(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        // Project gets short_id = 1; its auto-spawned design task gets 2.
        assert_eq!(project.short_id, Some(1));

        // `get_project` also surfaces the field.
        let fetched = db.get_project(&project.id).unwrap();
        assert_eq!(fetched.short_id, Some(1));

        let _ = std::fs::remove_file(path);
    }

    /// `list_tasks_for_product` (used by WorkTree / Subscribe) carries
    /// `short_id` in every returned `Task`.
    #[test]
    fn work_tree_tasks_carry_short_id() {
        let path = temp_db_path("short-id-wire-worktree");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
            })
            .unwrap();

        db.create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c1".into(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        let chore = &tree.chores[0];
        assert_eq!(chore.short_id, Some(1), "WorkTree chore carries short_id");

        let _ = std::fs::remove_file(path);
    }

    /// A no-op status patch (patch.status == current status) must NOT flip
    /// `last_status_actor` to 'human'. Regression test for the bug where
    /// `status_changed = patch.status.is_some()` caused any patch that
    /// carried a status field to overwrite the actor, silently disabling
    /// the engine's auto-unblock cascade.
    #[test]
    fn noop_status_patch_preserves_last_status_actor_for_task() {
        let path = temp_db_path("noop-status-actor-task");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "P".into(),
                description: None,
                repo_remote_url: Some("git@github.com:example/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "C".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        // Simulate the engine having set the status by writing directly.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET last_status_actor = 'engine' WHERE id = ?1",
                rusqlite::params![chore.id],
            )
            .unwrap();
        }

        // No-op status patch: same value the row already has ('todo').
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let conn = db.connect().unwrap();
        let actor: String = conn
            .query_row(
                "SELECT last_status_actor FROM tasks WHERE id = ?1",
                rusqlite::params![chore.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            actor, "engine",
            "no-op status patch must not flip last_status_actor from 'engine' to 'human'"
        );

        let _ = std::fs::remove_file(path);
    }

    /// Same invariant as `noop_status_patch_preserves_last_status_actor_for_task`
    /// but exercised on the project path.
    #[test]
    fn noop_status_patch_preserves_last_status_actor_for_project() {
        let path = temp_db_path("noop-status-actor-project");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Prod".into(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Proj".into(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        // Pre-seed last_status_actor = 'engine' directly.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE projects SET last_status_actor = 'engine' WHERE id = ?1",
                rusqlite::params![project.id],
            )
            .unwrap();
        }

        // No-op status patch: project default status is 'planned'.
        let current_status = project.status.clone();
        db.update_work_item(
            &project.id,
            WorkItemPatch {
                status: Some(current_status),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let conn = db.connect().unwrap();
        let actor: String = conn
            .query_row(
                "SELECT last_status_actor FROM projects WHERE id = ?1",
                rusqlite::params![project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            actor, "engine",
            "no-op status patch must not flip last_status_actor from 'engine' to 'human'"
        );

        let _ = std::fs::remove_file(path);
    }

    /// A genuine status change must still flip `last_status_actor` to 'human'.
    #[test]
    fn real_status_change_sets_last_status_actor_human_for_task() {
        let path = temp_db_path("real-status-actor-task");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "P".into(),
                description: None,
                repo_remote_url: Some("git@github.com:example/repo.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "C".into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET last_status_actor = 'engine' WHERE id = ?1",
                rusqlite::params![chore.id],
            )
            .unwrap();
        }

        // Genuine status change: todo → doing.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("doing".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let conn = db.connect().unwrap();
        let actor: String = conn
            .query_row(
                "SELECT last_status_actor FROM tasks WHERE id = ?1",
                rusqlite::params![chore.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            actor, "human",
            "genuine status change must flip last_status_actor to 'human'"
        );

        let _ = std::fs::remove_file(path);
    }
}
