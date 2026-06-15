use super::*;

impl WorkDb {
    pub(crate) fn init(&self) -> Result<()> {
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
                default_driver TEXT,
                ci_attempt_budget INTEGER NOT NULL DEFAULT 3,
                dispatch_preamble TEXT,
                external_tracker_kind TEXT,
                external_tracker_config TEXT,
                design_repo TEXT,
                worker_branch_prefix TEXT
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
                driver TEXT,
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
                finished_at TEXT,
                host_id TEXT NOT NULL DEFAULT 'local',
                cube_workspace_id TEXT,
                remote_pid INTEGER
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
        migrate_ci_remediations_failure_kind_columns(&conn)?;
        migrate_ci_failure_suppressions_table(&conn)?;
        migrate_ci_inflight_observations_table(&conn)?;
        migrate_tasks_ci_attempt_columns(&conn)?;
        migrate_products_ci_attempt_budget(&conn)?;
        migrate_products_dispatch_preamble(&conn)?;
        migrate_products_design_repo(&conn)?;
        migrate_products_docs_repo(&conn)?;
        migrate_products_worker_branch_prefix(&conn)?;
        migrate_work_executions_worker_branch_prefix(&conn)?;
        // The bespoke investigation-doc pointer columns are gone — the card
        // affordance now derives from `pr_url`, mirroring the design-doc model.
        // This drop is idempotent (fresh DBs never had the columns).
        migrate_drop_tasks_investigation_doc_columns(&conn)?;
        // Per-task doc-pointer columns (doc_repo_remote_url / doc_branch /
        // doc_path) for the project-less doc-link card affordance —
        // investigations have no project, so they cannot reuse the
        // per-project `design_doc_*` columns. Detector-populated from the
        // PR's changed files, mirroring the design-doc model.
        migrate_tasks_doc_pointer_columns(&conn)?;
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
        // Positive-evidence columns for the metadata-only CI-fix finalize
        // gate (issue #1252): the PR body snapshotted at run start plus the
        // Stop-boundary "metadata delta observed" marker.
        migrate_work_executions_metadata_fix_columns(&conn)?;
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
        // Phase 3: add host_id / cube_workspace_id / remote_pid to work_runs
        // so the macOS app (and run-failure paths) can see which host
        // a run executed on.
        crate::host_registry::migrate_work_runs_host_columns(&conn)?;
        crate::host_registry::ensure_local_host(&conn)?;
        crate::host_registry::refresh_local_host_auto_capabilities(&conn)?;
        // Revision tasks (Phase 1): parent linkage column + index on tasks,
        // and soft-prefer signal on work_executions. Ships dark — the
        // `revision` kind is parseable but not yet dispatchable.
        // Design: tools/boss/docs/designs/revision-tasks.md
        migrate_tasks_parent_task_id_column(&conn)?;
        migrate_work_executions_prefer_is_soft(&conn)?;
        migrate_work_executions_transient_failure_count(&conn)?;
        migrate_work_executions_allow_dirty(&conn)?;
        // Revision card fix: update existing revision rows whose `name` was
        // set to the full description text (the original insertion behaviour).
        // The new insertion code uses only the first line; this backfill
        // aligns pre-fix rows by truncating to the first newline-terminated
        // segment using SQLite string functions. Rows whose name already
        // differs from description (e.g. manually patched via `boss task edit`)
        // are intentionally skipped.
        migrate_revision_names_to_first_line(&conn)?;
        // Phase 1 of `unify-pr-remediation-on-revisions.md`: add the
        // `revision_task_id` reverse link to both attempt side-tables so
        // Phase 2+ can stamp the FK when a producer creates a revision.
        // Additive only — bespoke conflict/CI flows are untouched.
        migrate_conflict_resolutions_revision_task_id(&conn)?;
        migrate_ci_remediations_revision_task_id(&conn)?;
        // Comments in the markdown viewer (Phase 2): engine-backed comment
        // rows with W3C TextQuoteSelector anchors. Independent of every
        // other table; `CREATE TABLE IF NOT EXISTS` so order is irrelevant.
        // Design: tools/boss/docs/designs/comments-in-markdown-viewer.md
        migrate_work_comments_table(&conn)?;
        // Comments Phase 3: magic-wand dispatch audit trail.
        migrate_magic_wand_dispatches_table(&conn)?;
        // Comments Phase 4: PR-backed doc → Boss chore worker. Adds `chore_id`
        // to `magic_wand_dispatches` for audit linkage.
        migrate_magic_wand_dispatches_add_chore_id(&conn)?;
        // Automations foundation (maintenance-tasks.md): `automations`,
        // `automation_runs`, `automation_short_id_sequences` tables plus
        // `tasks.source_automation_id` provenance column. Purely additive —
        // no existing rows are touched and no behaviour changes ship with
        // this migration. Everything depends on these tables existing.
        migrate_automations_tables(&conn)?;
        migrate_tasks_source_automation_id(&conn)?;
        // Attentions — new `attention_groups` and `attentions` tables for
        // agent-raised, human-actionable notifications (questions +
        // followups). Design: tools/boss/docs/designs/attentions.md.
        migrate_attentions(&conn)?;
        // Editorial controls (P576, chore #1): per-product editorial_rules JSON
        // column, branch_naming snapshot on work_executions, and editorial_actions
        // audit table. Ships dark — no behaviour change until a product opts in.
        // Design: tools/boss/docs/designs/editorial-controls-for-agent-authored-prs-and-github-comments.md
        migrate_editorial_controls_schema(&conn)?;
        // Normalise any effort_level rows stored as '' to NULL. The mapper
        // already converts '' → None at read time, but canonical DB storage
        // should use NULL (consistent with schema intent and SQL IS NULL queries).
        migrate_tasks_empty_effort_to_null(&conn)?;
        // Behavior 8: upstream title/body drift detection. Adds
        // `external_ref_upstream_title` and `external_ref_upstream_body` to
        // `tasks` so the reconciler can tell apart operator edits from upstream
        // changes without parsing the description prose. Superseded by the
        // checksum migration below but kept for safe forward compatibility.
        migrate_external_tracker_upstream_content(&conn)?;
        // Behavior 8 (revision): replace raw-content columns with SHA-256
        // checksums. Adds `external_ref_upstream_checksum` and
        // `external_ref_boss_checksum`; the old title/body columns remain in
        // the schema but are no longer read or written.
        migrate_external_tracker_content_checksums(&conn)?;
        // P992 task 9: loop termination & bounds — per-PR review cycle
        // counter and last-reviewed SHA for the no-op skip gate.
        migrate_tasks_review_cycle_columns(&conn)?;
        // P783 task 2: planner_runs audit ledger + per-project idempotency gate.
        // The UNIQUE partial index is created here (after the table) so SQLite
        // can resolve the `outcome` column. `CREATE TABLE IF NOT EXISTS` +
        // `CREATE INDEX IF NOT EXISTS` make this fully idempotent.
        // Design: tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md
        migrate_planner_runs_table(&conn)?;
        // P1422 task B: driver data model (mix-and-match agent-driver
        // abstraction). Adds `tasks.driver` and `products.default_driver`
        // TEXT columns. NULL resolves to the engine default (`"claude"`).
        migrate_tasks_driver_column(&conn)?;
        migrate_products_default_driver(&conn)?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '20')
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
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .with_context(|| format!("failed to connect to in-memory db {}", mem.uri))?
        } else {
            Connection::open(&self.path).with_context(|| format!("failed to open work db {}", self.path.display()))?
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
}
