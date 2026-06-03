use super::*;

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert!(
        second.is_none(),
        "second insert on same key must be a no-op"
    );

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    for idx in ["tasks_external_ref_idx", "tasks_external_ref_bound_uniq"] {
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
    assert_eq!(version, "18");
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
    assert_eq!(version, "18");
    let _ = std::fs::remove_file(path);
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
    assert!(
        found.is_none(),
        "cleared row must not appear in find_by_external_ref"
    );
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
    assert!(
        unbound.unbound_at.is_some(),
        "cleared row must have unbound_at set"
    );

    let bound = refs
        .iter()
        .find(|(id, _)| id == &chore2.id)
        .map(|(_, r)| r)
        .unwrap();
    assert!(
        bound.unbound_at.is_none(),
        "active row must have no unbound_at"
    );
}

/// import_chore_with_external_ref creates the chore and binds the
/// external_ref in a single transaction: the chore must be immediately
/// findable via find_by_external_ref and have synced_at populated.
#[test]
fn import_chore_with_external_ref_is_atomic_and_findable() {
    let (db, product_id, _) = setup_product_and_chore();
    let raw = serde_json::json!({ "issue_number": 42 });
    let chore = db
        .import_chore_with_external_ref(
            CreateChoreInput {
                product_id: product_id.clone(),
                name: "Imported issue".into(),
                description: Some(
                    "> Imported from https://github.com/example/repo/issues/42\n\nBody text".into(),
                ),
                autostart: false,
                priority: None,
                created_via: Some(boss_protocol::CREATED_VIA_EXTERNAL_TRACKER_SYNC.to_owned()),
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: true,
            },
            "github",
            "example/repo#42",
            &raw,
            "Imported issue",
            "Body text",
        )
        .expect("import_chore_with_external_ref must succeed");

    // The chore must be immediately findable by external_ref
    // — no separate set_external_ref call required.
    let found = db
        .find_by_external_ref("github", "example/repo#42")
        .expect("query ok")
        .expect("chore must be findable by external_ref right after import");
    assert_eq!(found.id, chore.id);

    // external_ref must be populated with the correct fields.
    let ext = found.external_ref.expect("external_ref must be set");
    assert_eq!(ext.kind, "github");
    assert_eq!(ext.canonical_id, "example/repo#42");
    assert_eq!(ext.raw["issue_number"], 42);

    // synced_at must be set within the same transaction.
    assert!(
        ext.synced_at.is_some(),
        "synced_at must be set after import"
    );
}

/// get_work_tree populates external_ref on chores so the kanban card
/// can render the upstream-link affordance (T503 follow-up / T588).
#[test]
fn get_work_tree_includes_external_ref_on_chores() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let raw = serde_json::json!({ "issue_number": 561 });
    db.set_external_ref(&chore_id, "github", "spinyfin/mono#561", &raw)
        .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let chore = tree.chores.iter().find(|c| c.id == chore_id).unwrap();
    let ext = chore
        .external_ref
        .as_ref()
        .expect("external_ref must be populated in work tree");
    assert_eq!(ext.kind, "github");
    assert_eq!(ext.canonical_id, "spinyfin/mono#561");
    assert_eq!(ext.web_url, "https://github.com/spinyfin/mono/issues/561");
}

/// derive_external_ref_web_url derives correct GitHub URLs and returns
/// empty string for unknown trackers.
#[test]
fn derive_external_ref_web_url_github() {
    assert_eq!(
        derive_external_ref_web_url("github", "spinyfin/mono#560"),
        "https://github.com/spinyfin/mono/issues/560"
    );
    assert_eq!(
        derive_external_ref_web_url("github", "org/repo#1"),
        "https://github.com/org/repo/issues/1"
    );
    assert_eq!(derive_external_ref_web_url("unknown", "jira-BOSS-42"), "");
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let refs = db.list_external_refs_for_product(&product.id).unwrap();
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
            ("task_dep".to_owned(), "dependency".to_owned(), None,),
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
        .query_row("SELECT COUNT(*) FROM task_blocked_signals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(again, 2);
    let version: String = conn2
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "18");

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

/// Manual move out of `blocked: ci_failure` writes a
/// `ci_failure_suppressions` row keyed on the most recent
/// `ci_remediations` head sha and resets `ci_attempts_used`. The
/// suppression is scoped to one head sha — a fresh push (new sha)
/// invalidates it automatically per design §Q5.
#[test]
fn manual_override_writes_ci_failure_suppression() {
    let path = disk_db_path("ci-manual-override");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "P".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-manual".into(),
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
    let pr_url = "https://github.com/foo/bar/pull/77".to_owned();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Drive the chore into `blocked: ci_failure` via the engine
    // path so a real `ci_remediations` row exists with a head sha
    // the suppression can key against.
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore.id.clone(),
        pr_url: pr_url.clone(),
        pr_number: 77,
        head_branch: "feature".into(),
        head_sha_at_trigger: "head-aaa".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap();
    db.mark_chore_blocked_ci_failure(&chore.id, &pr_url, None)
        .unwrap();
    db.increment_ci_attempts_used(&chore.id).unwrap();
    assert!(db.get_ci_attempts_used(&chore.id).unwrap() >= 1);

    // Human pulls the chore back to `in_review`.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            blocked_reason: Some("".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert!(
        db.is_ci_failure_suppressed(&chore.id, "head-aaa").unwrap(),
        "suppression row must be keyed on the most recent ci_remediations head sha",
    );
    // Budget reset on manual override.
    assert_eq!(db.get_ci_attempts_used(&chore.id).unwrap(), 0);
    // Suppression is scoped to one head sha — a new push gets no
    // protection.
    assert!(!db.is_ci_failure_suppressed(&chore.id, "head-bbb").unwrap());
    let _ = std::fs::remove_file(path);
}

/// Same shape as the `ci_failure` override, but from the
/// `ci_failure_exhausted` state — the design treats both blocked
/// reasons as equivalent triggers for the suppression write.
#[test]
fn manual_override_from_exhausted_writes_suppression() {
    let path = disk_db_path("ci-manual-override-exh");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "P".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-exh".into(),
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
    let pr_url = "https://github.com/foo/bar/pull/78".to_owned();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore.id.clone(),
        pr_url: pr_url.clone(),
        pr_number: 78,
        head_branch: "feature".into(),
        head_sha_at_trigger: "head-ccc".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap();
    db.mark_chore_blocked_ci_failure_exhausted(&chore.id, &pr_url)
        .unwrap();

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            blocked_reason: Some("".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert!(db.is_ci_failure_suppressed(&chore.id, "head-ccc").unwrap());
    let _ = std::fs::remove_file(path);
}

/// Phase 12 #40 — the CI-retry churn guard fires once a work
/// item has accumulated >= `CI_CHURN_LIMIT` (5) `ci_remediations`
/// rows within the last `CI_CHURN_WINDOW_SECS` (1 h). The
/// `--force` override path always returns false so the caller can
/// still proceed after surfacing a loud warning.
#[test]
fn ci_retry_rate_limit_fires_after_five_attempts_in_one_hour() {
    let path = disk_db_path("ci-churn");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "P".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-churn".into(),
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

    // 0 attempts → not rate-limited.
    assert!(!db.is_ci_retry_rate_limited(&chore.id, false).unwrap());

    // Insert 4 attempts in the recent window — still under the
    // threshold (the engine only rate-limits once the count
    // reaches CI_CHURN_LIMIT = 5).
    for i in 0..4 {
        db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/40".into(),
            pr_number: 40,
            head_branch: "feature".into(),
            head_sha_at_trigger: format!("head-{i}"),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap();
    }
    assert_eq!(
        db.count_recent_ci_remediations(&chore.id, CI_CHURN_WINDOW_SECS)
            .unwrap(),
        4,
    );
    assert!(!db.is_ci_retry_rate_limited(&chore.id, false).unwrap());

    // 5th attempt trips the threshold.
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/40".into(),
        pr_number: 40,
        head_branch: "feature".into(),
        head_sha_at_trigger: "head-5".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap();
    assert!(
        db.is_ci_retry_rate_limited(&chore.id, false).unwrap(),
        "5 attempts in the window must rate-limit the next retry",
    );

    // `--force` override path always returns false.
    assert!(!db.is_ci_retry_rate_limited(&chore.id, true).unwrap());

    // Attempts older than the 1h window do not count. Rewrite the
    // created_at on every existing row to two hours ago and the
    // guard should drop back to off.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let two_hours_ago = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 2 * CI_CHURN_WINDOW_SECS)
        .to_string();
    conn.execute(
        "UPDATE ci_remediations SET created_at = ?1 WHERE work_item_id = ?2",
        rusqlite::params![two_hours_ago, &chore.id],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        db.count_recent_ci_remediations(&chore.id, CI_CHURN_WINDOW_SECS)
            .unwrap(),
        0,
    );
    assert!(!db.is_ci_retry_rate_limited(&chore.id, false).unwrap());

    let _ = std::fs::remove_file(path);
}

/// Manual moves between non-CI states must NOT touch the
/// suppression table — only moves OUT of `blocked: ci_failure` /
/// `ci_failure_exhausted` are an override signal.
#[test]
fn manual_move_unrelated_to_ci_does_not_write_suppression() {
    let path = disk_db_path("ci-manual-override-noop");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "P".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-noop".into(),
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
    let pr_url = "https://github.com/foo/bar/pull/79".to_owned();
    // to_do → in_review → to_do with no CI involvement. None of
    // these transitions should reach the suppression code path
    // because the previous `blocked_reason` is never a CI reason.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("to_do".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM ci_failure_suppressions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        n, 0,
        "non-CI moves must not write to ci_failure_suppressions"
    );
    let _ = std::fs::remove_file(path);
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
        design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
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
        design_doc_repo_remote_url: Some("  https://github.com/myorg/wiki.git  ".to_owned()),
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
        .resolve_project_design_doc(&project.id, |_| Some("/tmp/mono-agent-007".to_owned()))
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
        Some(
            "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=main"
        ),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();

    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
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
        Some("https://raw.githubusercontent.com/myorg/wiki/designs/foo.md?ref=docs"),
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
        design_doc_repo_remote_url: Some("https://github.com/external/other.git".to_owned()),
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
    let ProjectDesignDocState::Resolved {
        raw_content_url,
        web_url,
        ..
    } = resolved.state
    else {
        panic!("expected Resolved, got {:?}", resolved.state);
    };

    assert_eq!(
        raw_content_url.as_deref(),
        Some(
            "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=design-boss-ci-buildkite"
        ),
        "SSH remote URL must produce a raw_content_url on a non-main branch"
    );
    assert_eq!(
        web_url,
        "https://github.com/spinyfin/mono/blob/design-boss-ci-buildkite/tools/boss/docs/designs/foo.md",
    );

    let _ = std::fs::remove_file(path);
}

/// Regression for the root cause of the unmerged-PR rendering failure:
/// `boss/exec_*` branch names contain `/`, which URL path-component
/// splitting in the Swift app would split into separate segments,
/// causing the `gh api` call to resolve `ref=boss` (not the full
/// `boss/exec_*`) and return 404. The fix encodes `/` as `%2F` in
/// the `?ref=` query param so the full branch name is preserved.
#[test]
fn resolve_project_design_doc_raw_content_url_encodes_slashed_branch() {
    let path = temp_db_path("resolve-raw-content-slashed-branch");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_, project) = seed_project_for_design_doc(&db);

    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: None,
        design_doc_branch: Some("boss/exec_18b07a506d2518d0_1b".to_owned()),
        design_doc_path: Some("tools/boss/docs/designs/foo.md".to_owned()),
        unset: false,
    })
    .unwrap();

    let resolved = db
        .resolve_project_design_doc(&project.id, |_| None)
        .unwrap();
    let ProjectDesignDocState::Resolved {
        raw_content_url, ..
    } = resolved.state
    else {
        panic!("expected Resolved, got {:?}", resolved.state);
    };

    assert_eq!(
        raw_content_url.as_deref(),
        Some(
            "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=boss%2Fexec_18b07a506d2518d0_1b"
        ),
        "slashed branch must be %2F-encoded in the ?ref= query param"
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

    db.set_project_design_doc(set_design_doc_input(&project.id, "designs/foo.md"))
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

/// Regression for the P844/T845 bug: a design doc that lives under a
/// non-boss product directory (e.g. `tools/checkleft/docs/designs/`) must
/// resolve to the PR head branch while the PR is still open, not to "main".
///
/// Root cause: `is_design_doc_path` previously rejected paths that did not
/// start with `tools/boss/docs/designs/`, so `on_design_pr_detected` returned
/// early without updating `design_doc_branch` to the PR head. The column
/// stayed `NULL`, and resolution fell back to "main" (the
/// `unwrap_or_else(|| "main".to_owned())` default), causing a 404 when the
/// app tried to load the doc from the default branch while it only existed
/// on the PR branch.
///
/// After the fix `is_design_doc_path` matches any `docs/designs/*.md` path,
/// so the detector sets the branch to the PR head regardless of the product
/// prefix.  This test verifies the full resolution round-trip given that the
/// pointer was populated with the PR head branch.
#[test]
fn resolve_project_design_doc_returns_pr_head_branch_for_non_boss_product_path() {
    let path = temp_db_path("resolve-pr-head-non-boss");
    let db = WorkDb::open(path.clone()).unwrap();
    // seed_project_for_design_doc uses `git@github.com:spinyfin/mono.git`.
    let (_, project) = seed_project_for_design_doc(&db);

    // Simulate what on_design_pr_detected now does: populate the pointer
    // with the PR head branch. The doc lives under checkleft's design dir.
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: None,
        design_doc_branch: Some("boss/exec_18b3fffb232a8060_ec".to_owned()),
        design_doc_path: Some(
            "tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md".to_owned(),
        ),
        unset: false,
    })
    .unwrap();

    let resolved = db
        .resolve_project_design_doc(&project.id, |_| None)
        .unwrap();
    let ProjectDesignDocState::Resolved {
        resolved,
        raw_content_url,
        web_url,
        ..
    } = resolved.state
    else {
        panic!("expected Resolved, got {:?}", resolved.state);
    };

    assert_eq!(
        resolved.branch, "boss/exec_18b3fffb232a8060_ec",
        "unmerged design PR must resolve to the PR head branch, not main"
    );
    assert_eq!(
        resolved.path,
        "tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md",
    );
    // Slashed branch must be %2F-encoded; URL must use the PR head branch.
    assert_eq!(
        raw_content_url.as_deref(),
        Some(
            "https://raw.githubusercontent.com/spinyfin/mono/tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md?ref=boss%2Fexec_18b3fffb232a8060_ec"
        ),
    );
    assert_eq!(
        web_url,
        "https://github.com/spinyfin/mono/blob/boss/exec_18b3fffb232a8060_ec/tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md",
    );

    let _ = std::fs::remove_file(path);
}

/// Fresh init and migration both land the editorial-controls schema
/// (P576 chore #1): `products.editorial_rules`, `work_executions.branch_naming`,
/// and the `editorial_actions` table + index. Existing rows must read
/// back as NULL (all-defaults, no behaviour change).
#[test]
fn fresh_init_includes_editorial_controls_schema() {
    let path = temp_db_path("editorial-fresh");
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();

    assert!(
        table_has_column(&conn, "products", "editorial_rules").unwrap(),
        "missing products.editorial_rules",
    );
    assert!(
        table_has_column(&conn, "work_executions", "branch_naming").unwrap(),
        "missing work_executions.branch_naming",
    );
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name = 'editorial_actions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_exists, 1, "editorial_actions table must exist");
    let idx_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_editorial_actions_product'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(idx_exists, 1, "idx_editorial_actions_product must exist");

    let _ = std::fs::remove_file(path);
}

/// Migration from a pre-editorial-controls schema adds the columns and
/// table idempotently; pre-existing product rows read back as NULL.
#[test]
fn migration_adds_editorial_controls_columns() {
    let path = disk_db_path("editorial-migrate");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE products (
             id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
             description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
             status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
         INSERT INTO products(id, name, slug, status, created_at, updated_at)
         VALUES ('prod_e', 'Editorial', 'editorial', 'active', '1700000000', '1700000000');
         INSERT INTO metadata(key, value) VALUES ('schema_version','4');",
    )
    .unwrap();
    drop(conn);

    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();

    assert!(
        table_has_column(&conn, "products", "editorial_rules").unwrap(),
        "migration must add products.editorial_rules",
    );
    assert!(
        table_has_column(&conn, "work_executions", "branch_naming").unwrap(),
        "migration must add work_executions.branch_naming",
    );

    // Pre-existing product row must read back as NULL (no behaviour change).
    let editorial_rules: Option<String> = conn
        .query_row(
            "SELECT editorial_rules FROM products WHERE id = 'prod_e'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        editorial_rules.is_none(),
        "existing row must have NULL editorial_rules",
    );

    // Idempotency: a second open must not fail.
    drop(conn);
    let _db2 = WorkDb::open(path.clone()).unwrap();

    let _ = std::fs::remove_file(path);
}
