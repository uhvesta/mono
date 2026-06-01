use super::*;

/// Add the `autostart` column to `tasks` for older databases. New
/// chores opt out of auto-dispatch by setting this column to 0;
/// `task_accepts_execution` then keeps them out of the reconcile loop
/// while their status is `todo`. Older rows default to 1 so the
/// historical "create-and-dispatch" behaviour is preserved.
pub(crate) fn migrate_tasks_autostart(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_last_status_actor(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_tasks_priority(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_tasks_repo_remote_url(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_tasks_created_via(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_project_design_doc_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_project_property_audit_table(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_backfill_project_design_tasks(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_tasks_blocked_reason(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_products_auto_pr_maintenance_enabled(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_conflict_resolutions_table(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_work_attention_items_work_item_id(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_tasks_effort_and_model_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_products_default_model(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "default_model")? {
        conn.execute("ALTER TABLE products ADD COLUMN default_model TEXT", [])?;
    }
    Ok(())
}

pub(crate) fn migrate_backfill_blocked_reason_dependency(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_task_blocked_signals_table(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_ci_remediations_table(conn: &Connection) -> Result<()> {
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
/// Add `failure_kind` and `before_commit_sha` columns to `ci_remediations`.
/// `failure_kind` defaults to `'pr_branch_ci'` so pre-migration rows read as
/// the normal per-PR CI path. `before_commit_sha` is NULL for those rows.
pub(crate) fn migrate_ci_remediations_failure_kind_columns(conn: &Connection) -> Result<()> {
    let has_fk: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('ci_remediations') WHERE name = 'failure_kind'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_fk {
        conn.execute_batch(
            "ALTER TABLE ci_remediations ADD COLUMN failure_kind TEXT NOT NULL DEFAULT 'pr_branch_ci';
             ALTER TABLE ci_remediations ADD COLUMN before_commit_sha TEXT;",
        )?;
    }
    Ok(())
}

pub(crate) fn migrate_ci_failure_suppressions_table(conn: &Connection) -> Result<()> {
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

/// Create the `ci_inflight_observations` table — observation log used
/// by the Phase 12 #39 "never-starts" alert path. One row per
/// `(work_item_id, head_sha)` pair: `first_observed_at` is stamped on
/// the probe that first reported `OpenPrCiStatus::InFlight` for the
/// pair, and `alert_level_emitted` records the highest log/event
/// bucket the engine has already raised so we don't spam the activity
/// feed on every subsequent poll. Rows are scoped to one head sha —
/// a new push invalidates them implicitly (the next probe inserts a
/// fresh row keyed on the new head sha); the engine also clears the
/// row when CI moves off InFlight (Clean → retire path; Failing →
/// detect path).
pub(crate) fn migrate_ci_inflight_observations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ci_inflight_observations (
             work_item_id        TEXT NOT NULL,
             head_sha            TEXT NOT NULL,
             first_observed_at   TEXT NOT NULL,
             alert_level_emitted TEXT NOT NULL DEFAULT 'none',
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
pub(crate) fn migrate_tasks_ci_attempt_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_products_ci_attempt_budget(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_products_dispatch_preamble(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "dispatch_preamble")? {
        conn.execute("ALTER TABLE products ADD COLUMN dispatch_preamble TEXT", [])?;
    }
    Ok(())
}

/// Add `products.design_repo` — a per-product override that points
/// `kind = 'design'` tasks at a different repo from the product's
/// implementation default (`repo_remote_url`). `NULL` → no override;
/// design tasks resolve through the standard chain. Implementation
/// kinds (`task`, `chore`, `project_task`) are unaffected.
pub(crate) fn migrate_products_design_repo(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "design_repo")? {
        conn.execute("ALTER TABLE products ADD COLUMN design_repo TEXT", [])?;
    }
    Ok(())
}

/// Add `products.docs_repo` — per-product target repo for
/// `kind = 'investigation'` deliverables. `NULL` → fall through to
/// `BOSS_USER_DOCS_REPO` env var at dispatch time.
pub(crate) fn migrate_products_docs_repo(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "docs_repo")? {
        conn.execute("ALTER TABLE products ADD COLUMN docs_repo TEXT", [])?;
    }
    Ok(())
}

/// Add `products.worker_branch_prefix` — the per-product leading prefix
/// for worker branch names (`<prefix>exec_<id>`). `NULL` → engine
/// default `boss/`. Lets orgs that enforce per-developer branch
/// prefixes via local hooks (e.g. `bduff/`) configure the prefix while
/// keeping the `exec_<id>` suffix that subsystems key off. Idempotent.
pub(crate) fn migrate_products_worker_branch_prefix(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "worker_branch_prefix")? {
        conn.execute(
            "ALTER TABLE products ADD COLUMN worker_branch_prefix TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Add `work_executions.worker_branch_prefix` — the worker branch-name
/// prefix frozen onto the execution at creation time, denormalised
/// from the owning product (same pattern as `repo_remote_url`). `NULL`
/// → engine default `boss/`. Freezing it keeps the engine-supplied
/// branch name reconstructible from `state.db` alone. Idempotent.
pub(crate) fn migrate_work_executions_worker_branch_prefix(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "worker_branch_prefix")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN worker_branch_prefix TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Drop the bespoke `tasks.investigation_doc_path` / `investigation_doc_branch`
/// pointer columns. The investigation-doc card affordance is now derived from
/// the task's `pr_url` — exactly like the design-doc affordance — so the
/// worker-set pointer triple is dead weight. Idempotent: a fresh database
/// never had these columns (they were only ever added by migration, never in
/// the `CREATE TABLE`), and an already-migrated database has them dropped here.
/// Any stored pointer values are intentionally discarded; the affordance reads
/// `pr_url` instead, so nothing downstream needs them.
pub(crate) fn migrate_drop_tasks_investigation_doc_columns(conn: &Connection) -> Result<()> {
    if table_has_column(conn, "tasks", "investigation_doc_path")? {
        conn.execute("ALTER TABLE tasks DROP COLUMN investigation_doc_path", [])?;
    }
    if table_has_column(conn, "tasks", "investigation_doc_branch")? {
        conn.execute("ALTER TABLE tasks DROP COLUMN investigation_doc_branch", [])?;
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
pub(crate) fn migrate_backfill_task_blocked_signals(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_effort_escalations_table(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_null_redundant_task_repo_remote_urls(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_short_id_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_backfill_autostart_consumed(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_pr_poll_state_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_external_tracker_columns(conn: &Connection) -> Result<()> {
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
pub(crate) fn migrate_metrics_tables(conn: &Connection) -> Result<()> {
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

/// Add `revision_task_id` to `conflict_resolutions` — the soft FK from a
/// trigger-ledger row to the `kind=revision` task the merge-conflict producer
/// spawned. `NULL` for rows written before Phase 2 of the unify-pr-remediation
/// design (`unify-pr-remediation-on-revisions.md`) and for attempts that were
/// retired without creating a revision.
pub(crate) fn migrate_conflict_resolutions_revision_task_id(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "conflict_resolutions", "revision_task_id")? {
        conn.execute(
            "ALTER TABLE conflict_resolutions ADD COLUMN revision_task_id TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Add `revision_task_id` to `ci_remediations` — the soft FK from a
/// trigger-ledger row to the `kind=revision` task the CI-failure producer
/// spawned. `NULL` for rows written before Phase 2 of the unify-pr-remediation
/// design and for `retrigger` kind attempts (which never spawn a revision).
pub(crate) fn migrate_ci_remediations_revision_task_id(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "ci_remediations", "revision_task_id")? {
        conn.execute(
            "ALTER TABLE ci_remediations ADD COLUMN revision_task_id TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Create the `magic_wand_dispatches` table for Phase 3 of
/// comments-in-markdown-viewer. Each row records one specialised Claude call
/// dispatched when the user clicks the magic-wand button on a comment against
/// a work-item description. Idempotent — `CREATE TABLE / INDEX IF NOT EXISTS`.
/// Design: `tools/boss/docs/designs/comments-in-markdown-viewer.md`
/// § Engine schema (magic_wand_dispatches).
pub(crate) fn migrate_magic_wand_dispatches_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS magic_wand_dispatches (
             id            TEXT PRIMARY KEY,
             comment_id    TEXT NOT NULL REFERENCES work_comments(id),
             artifact_kind TEXT NOT NULL,
             artifact_id   TEXT NOT NULL,
             doc_version   TEXT NOT NULL,
             status        TEXT NOT NULL,
             input_tokens  INTEGER,
             output_tokens INTEGER,
             result_md     TEXT,
             error_kind    TEXT,
             anchor_warning INTEGER NOT NULL DEFAULT 0,
             created_at    TEXT NOT NULL,
             resolved_at   TEXT
         );
         CREATE INDEX IF NOT EXISTS magic_wand_dispatches_by_comment
             ON magic_wand_dispatches(comment_id, created_at);",
    )?;
    Ok(())
}

/// Add the `chore_id` column to `magic_wand_dispatches` for Phase 4 of
/// comments-in-markdown-viewer (PR-backed doc → Boss chore worker).
///
/// Idempotent — uses `ALTER TABLE … ADD COLUMN IF NOT EXISTS` pattern via
/// a guarded SELECT-from-pragma approach, because SQLite's `ADD COLUMN IF
/// NOT EXISTS` syntax is not available until SQLite 3.37.0 and we prefer
/// to stay on the safe side.
pub(crate) fn migrate_magic_wand_dispatches_add_chore_id(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('magic_wand_dispatches') WHERE name = 'chore_id'")?
        .exists([])?;
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE magic_wand_dispatches ADD COLUMN chore_id TEXT;",
        )?;
    }
    Ok(())
}

/// Create the `work_comments` table for the comments-in-markdown-viewer
/// feature (Phase 2). Idempotent — `CREATE TABLE / INDEX IF NOT EXISTS`,
/// safe to re-run on every engine start. Schema follows design
/// `comments-in-markdown-viewer.md` § "Engine schema", extended with two
/// Phase-2 columns the design flagged for implementation:
///
/// - `last_resolved_with` records the most recent anchor-resolution mode
///   (`exact` / `fuzzy` / `orphan`) so the sidebar can show the ⚠ glyph when
///   a comment re-anchored fuzzily (§ Risks mitigation).
/// - `plain_text_projection_version` records the renderer's projection
///   algorithm version so a future projection upgrade can mass re-anchor
///   (§ Risks mitigation).
///
/// `anchor_json` holds the serialised `{exact, prefix, suffix}` selector.
/// Timestamps are TEXT epoch-seconds, matching every other table.
pub(crate) fn migrate_work_comments_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_comments (
             id                            TEXT PRIMARY KEY,
             artifact_kind                 TEXT NOT NULL,
             artifact_id                   TEXT NOT NULL,
             doc_version                   TEXT NOT NULL,
             anchor_json                   TEXT NOT NULL,
             body                          TEXT NOT NULL,
             author                        TEXT NOT NULL,
             status                        TEXT NOT NULL,
             status_actor                  TEXT,
             last_resolved_with            TEXT,
             plain_text_projection_version INTEGER NOT NULL DEFAULT 0,
             created_at                    TEXT NOT NULL,
             updated_at                    TEXT NOT NULL,
             dismissed_at                  TEXT
         );
         CREATE INDEX IF NOT EXISTS work_comments_by_artifact
             ON work_comments(artifact_kind, artifact_id, status);",
    )?;
    Ok(())
}

/// Create the `automations` and `automation_runs` tables plus the
/// `automation_short_id_sequences` counter table. Idempotent — all
/// DDL uses `CREATE TABLE IF NOT EXISTS` / `CREATE … IF NOT EXISTS`.
///
/// Design: `tools/boss/docs/designs/maintenance-tasks.md` §"Data model".
pub(crate) fn migrate_automations_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS automations (
             id                    TEXT PRIMARY KEY,
             short_id              INTEGER,
             product_id            TEXT NOT NULL REFERENCES products(id),
             name                  TEXT NOT NULL,
             repo_remote_url       TEXT,
             trigger_kind          TEXT NOT NULL,
             trigger_config        TEXT NOT NULL,
             standing_instruction  TEXT NOT NULL,
             open_task_limit       INTEGER NOT NULL DEFAULT 1,
             catch_up_window_secs  INTEGER,
             enabled               INTEGER NOT NULL DEFAULT 1,
             created_via           TEXT NOT NULL DEFAULT 'unknown',
             created_at            TEXT NOT NULL,
             updated_at            TEXT NOT NULL,
             last_fired_at         TEXT,
             last_outcome          TEXT,
             next_due_at           TEXT
         );

         CREATE UNIQUE INDEX IF NOT EXISTS automations_product_short_id_idx
             ON automations(product_id, short_id) WHERE short_id IS NOT NULL;

         CREATE INDEX IF NOT EXISTS automations_due_idx
             ON automations(enabled, next_due_at);

         CREATE TABLE IF NOT EXISTS automation_runs (
             id                   TEXT PRIMARY KEY,
             automation_id        TEXT NOT NULL REFERENCES automations(id),
             scheduled_for        TEXT NOT NULL,
             started_at           TEXT NOT NULL,
             finished_at          TEXT,
             triage_execution_id  TEXT,
             outcome              TEXT NOT NULL,
             produced_task_id     TEXT REFERENCES tasks(id),
             detail               TEXT
         );

         CREATE INDEX IF NOT EXISTS automation_runs_by_automation_idx
             ON automation_runs(automation_id, scheduled_for);

         CREATE TABLE IF NOT EXISTS automation_short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );",
    )?;
    Ok(())
}

/// Add `tasks.source_automation_id` — a soft FK to `automations.id`
/// that marks tasks produced by the automations triage flow. `NULL` for
/// every existing task row; non-`NULL` only on tasks created via
/// `boss task create --automation`. The partial index enables cheap
/// open-task-count queries and backlog/kanban exclusion filters.
pub(crate) fn migrate_tasks_source_automation_id(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "source_automation_id")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN source_automation_id TEXT REFERENCES automations(id)",
            [],
        )?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS tasks_source_automation_idx
             ON tasks(source_automation_id, status)
          WHERE source_automation_id IS NOT NULL",
        [],
    )?;
    Ok(())
}

/// Add editorial-controls schema (P576, chore #1).
///
/// - `products.editorial_rules` (TEXT, NULL): JSON blob of per-product
///   editorial rules injected into worker prompts. NULL means no rules
///   (all-defaults). Mirrors the opaque-JSON pattern used by
///   `external_tracker_config`.
/// - `work_executions.branch_naming` (TEXT, NULL): branch-naming
///   convention snapshot taken at spawn time. NULL means the legacy
///   default ("boss_exec_prefix"). Denormalised from product so the
///   execution is self-describing even if the product is later edited.
/// - `editorial_actions` table: append-only audit log of allow/rewrite/
///   deny decisions made by the editorial pass (Phase 2+). Written dark
///   in this migration — no engine code reads or writes it yet.
///
/// Idempotent: column additions are guarded by `table_has_column`;
/// the table and index use `CREATE … IF NOT EXISTS`.
///
/// Design: `tools/boss/docs/designs/editorial-controls-for-agent-authored-prs-and-github-comments.md`
pub(crate) fn migrate_editorial_controls_schema(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "products", "editorial_rules")? {
        conn.execute(
            "ALTER TABLE products ADD COLUMN editorial_rules TEXT",
            [],
        )?;
    }
    if !table_has_column(conn, "work_executions", "branch_naming")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN branch_naming TEXT",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS editorial_actions (
             id           INTEGER PRIMARY KEY,
             product_id   TEXT NOT NULL REFERENCES products(id),
             execution_id TEXT,
             pr_url       TEXT,
             tool_command TEXT NOT NULL,
             action       TEXT NOT NULL CHECK (action IN ('allow', 'rewrite', 'deny')),
             reason       TEXT,
             created_at   TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_editorial_actions_product
             ON editorial_actions(product_id, created_at DESC);",
    )?;
    Ok(())
}

/// Create the `attention_groups` and `attentions` tables for the
/// Attentions feature (design: `tools/boss/docs/designs/attentions.md`).
///
/// `attention_groups` (`atg_…` ids, per-product `A<n>` short id) is the
/// human-actionable unit; `attentions` (`atn_…` ids) are its members.
/// Idempotent — all DDL uses `IF NOT EXISTS`.
///
/// Unique index on `(grouping_key, generation)` is the reconciliation
/// upsert target: re-running a design worker that emits the same questions
/// is a no-op. Partial-unique index on `(product_id, short_id)` mirrors
/// the tasks/projects pattern. FK `attentions.group_id → attention_groups`
/// with `ON DELETE CASCADE` keeps orphan rows from accumulating.
///
/// `attention_group_short_id_sequences` is the per-product `A<n>` counter
/// (parallel to `automation_short_id_sequences`), giving groups a dense
/// per-product short id independent of the tasks/projects counter.
pub(crate) fn migrate_attentions(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS attention_groups (
             id                         TEXT PRIMARY KEY,
             product_id                 TEXT NOT NULL REFERENCES products(id),
             short_id                   INTEGER,
             kind                       TEXT NOT NULL,
             association_project_id     TEXT REFERENCES projects(id),
             association_task_id        TEXT REFERENCES tasks(id),
             source_kind                TEXT NOT NULL,
             source_task_id             TEXT,
             source_run_id              TEXT,
             source_doc_path            TEXT,
             source_doc_repo_remote_url TEXT,
             source_doc_branch          TEXT,
             grouping_key               TEXT NOT NULL,
             generation                 INTEGER NOT NULL DEFAULT 0,
             state                      TEXT NOT NULL DEFAULT 'open',
             produced_artifact_kind     TEXT,
             produced_artifact_ref      TEXT,
             created_at                 TEXT NOT NULL,
             actioned_at                TEXT,
             dismissed_at               TEXT,
             CHECK (
                 (association_project_id IS NOT NULL AND association_task_id IS NULL)
                 OR (association_project_id IS NULL  AND association_task_id IS NOT NULL)
             )
         );
         CREATE UNIQUE INDEX IF NOT EXISTS attention_groups_grouping_key_idx
             ON attention_groups(grouping_key, generation);
         CREATE UNIQUE INDEX IF NOT EXISTS attention_groups_product_short_id_idx
             ON attention_groups(product_id, short_id)
             WHERE short_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS attention_groups_product_state_idx
             ON attention_groups(product_id, state, created_at);

         CREATE TABLE IF NOT EXISTS attentions (
             id                  TEXT PRIMARY KEY,
             group_id            TEXT NOT NULL
                                     REFERENCES attention_groups(id) ON DELETE CASCADE,
             ordinal             INTEGER NOT NULL,
             source_anchor       TEXT,
             answer_state        TEXT NOT NULL DEFAULT 'open',
             created_at          TEXT NOT NULL,
             answered_at         TEXT,
             question_type       TEXT,
             prompt_text         TEXT,
             choice_options      TEXT,
             answer              TEXT,
             proposed_name       TEXT,
             proposed_description TEXT,
             proposed_effort     TEXT,
             proposed_work_kind  TEXT,
             rationale           TEXT,
             confidence_source   TEXT NOT NULL DEFAULT 'structured'
         );
         CREATE INDEX IF NOT EXISTS attentions_group_idx
             ON attentions(group_id, ordinal);
         CREATE TABLE IF NOT EXISTS attention_group_short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );",
    )?;
    Ok(())
}
