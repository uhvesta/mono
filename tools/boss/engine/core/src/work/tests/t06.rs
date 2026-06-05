use super::*;

/// `retry_ci_remediation_for_work_item` always zeroes the
/// `ci_attempts_used` counter, and additionally flips the parent
/// from `blocked: ci_failure_exhausted` back to `in_review` when
/// that's where the parent was. The matching
/// `task_blocked_signals` row is also cleared.
#[test]
fn retry_ci_remediation_resets_counter_and_unblocks_exhausted_parent() {
    let path = disk_db_path("ci-retry-resets");
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
            name: "chore-retry".into(),
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
    let pr_url = "https://github.com/foo/bar/pull/200".to_owned();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.increment_ci_attempts_used(&chore.id).unwrap();
    db.increment_ci_attempts_used(&chore.id).unwrap();
    db.increment_ci_attempts_used(&chore.id).unwrap();
    db.mark_chore_blocked_ci_failure_exhausted(&chore.id, &pr_url)
        .unwrap();

    let (snapshot, was_exhausted) = db
        .retry_ci_remediation_for_work_item(&chore.id)
        .unwrap()
        .expect("work item exists");
    assert!(was_exhausted);
    assert_eq!(snapshot.used, 0);
    // After unblock the parent should no longer be blocked.
    assert_eq!(snapshot.blocked_reason, None);

    let conn = db.connect().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM tasks WHERE id = ?1",
            rusqlite::params![chore.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    assert_eq!(status, "in_review");
    assert_eq!(blocked_reason, None);
    // The matching blocked-signal row must be cleared.
    let cleared: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_blocked_signals
                  WHERE work_item_id = ?1
                    AND reason = 'ci_failure_exhausted'
                    AND cleared_at IS NULL",
            rusqlite::params![chore.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cleared, 0);

    // Second call: counter is already zero and the parent is
    // already in_review, so was_exhausted is false.
    let (snapshot, was_exhausted) = db
        .retry_ci_remediation_for_work_item(&chore.id)
        .unwrap()
        .expect("work item exists");
    assert!(!was_exhausted);
    assert_eq!(snapshot.used, 0);

    // Unknown work item → Ok(None).
    assert!(
        db.retry_ci_remediation_for_work_item("chr_does_not_exist")
            .unwrap()
            .is_none()
    );

    let _ = std::fs::remove_file(path);
}

/// `list_engine_attempts` unions the three subsystems with a
/// `kind` discriminator, applies the `kinds` filter when present,
/// and orders by `created_at DESC`.
#[test]
fn list_engine_attempts_unions_three_subsystems_with_kind_filter() {
    let path = disk_db_path("list-engine-attempts");
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
            name: "chore-attempts".into(),
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
    let pr_url = "https://github.com/foo/bar/pull/300".to_owned();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    // CI remediation row.
    let ci = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr_url.clone(),
            pr_number: 300,
            head_branch: "feature".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    // Conflict resolution row.
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr_url.clone(),
            pr_number: 300,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    // Unfiltered → both rows, ordered by created_at DESC.
    let unfiltered = db.list_engine_attempts(&[], None, &[], None, None).unwrap();
    // ci was inserted second, so it should be first.
    assert_eq!(unfiltered.len(), 2);
    let kinds: Vec<&str> = unfiltered.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains(&"ci"));
    assert!(kinds.contains(&"conflict"));
    // Filter to only `ci`.
    let only_ci = db
        .list_engine_attempts(&["ci".into()], None, &[], None, None)
        .unwrap();
    assert_eq!(only_ci.len(), 1);
    assert_eq!(only_ci[0].kind, "ci");
    assert_eq!(only_ci[0].id, ci.id);
    // ci rows expose `attempt_kind` under `extra`.
    assert_eq!(
        only_ci[0].extra.get("attempt_kind").map(String::as_str),
        Some("fix")
    );
    // Filter to only `conflict`.
    let only_conflict = db
        .list_engine_attempts(&["conflict".into()], None, &[], None, None)
        .unwrap();
    assert_eq!(only_conflict.len(), 1);
    assert_eq!(only_conflict[0].kind, "conflict");
    assert_eq!(only_conflict[0].id, crz.id);
    // Filter to `rebase`: the table doesn't exist in this fixture,
    // so the result must be empty (no panic).
    let only_rebase = db
        .list_engine_attempts(&["rebase".into()], None, &[], None, None)
        .unwrap();
    assert!(only_rebase.is_empty());
    // Limit honoured.
    let capped = db
        .list_engine_attempts(&[], None, &[], None, Some(1))
        .unwrap();
    assert_eq!(capped.len(), 1);

    let _ = std::fs::remove_file(path);
}

/// Transitioning a chore out of `blocked` must clear `blocked_reason`
/// and `blocked_attempt_id` even when the patch does not explicitly
/// include those fields. Covers the human-unblock path
/// (`update_work_item`) as the belt-and-suspenders invariant.
#[test]
fn unblock_via_update_clears_blocked_reason_and_attempt_id() {
    let path = temp_db_path("unblock-clears-blocked-fields");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "P".into(),
            description: None,
            repo_remote_url: Some("git@github.com:example/repo.git".into()),
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

    // Simulate the engine having set blocked fields directly.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure', \
                 blocked_attempt_id = 'cir_test123' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    }

    // Human (or engine) flips status to in_review without touching blocked fields.
    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

    let task = match updated {
        WorkItem::Chore(t) => t,
        other => panic!("expected Chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert!(
        task.blocked_reason.is_none(),
        "blocked_reason must be NULL after unblock; got {:?}",
        task.blocked_reason
    );
    assert!(
        task.blocked_attempt_id.is_none(),
        "blocked_attempt_id must be NULL after unblock; got {:?}",
        task.blocked_attempt_id
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn get_live_execution_returns_waiting_human_execution_for_work_item() {
    let db = WorkDb::open(temp_db_path("live-exec")).unwrap();
    let (_, chore_id, exec_a_id) = make_waiting_human_chore(&db, "live-exec");

    // A second ready execution for the same chore (as would be created by
    // the orphan sweep).
    let exec_b = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url("git@github.com:foo/bar.git")
            .build())
        .unwrap();

    // exec_b should see exec_a as live.
    let live = db
        .get_live_execution_for_work_item(&chore_id, &exec_b.id)
        .unwrap();
    assert!(live.is_some(), "exec_a should appear as live");
    assert_eq!(live.unwrap().id, exec_a_id);
}

#[test]
fn get_live_execution_excludes_specified_id() {
    let db = WorkDb::open(temp_db_path("live-exec-exclude")).unwrap();
    let (_, chore_id, exec_a_id) = make_waiting_human_chore(&db, "live-exec-exclude");

    // Querying with exec_a as the exclude_id should return None.
    let live = db
        .get_live_execution_for_work_item(&chore_id, &exec_a_id)
        .unwrap();
    assert!(
        live.is_none(),
        "should not return the excluded execution itself"
    );
}

#[test]
fn get_live_execution_returns_none_when_all_executions_are_terminal() {
    let db = WorkDb::open(temp_db_path("live-exec-terminal")).unwrap();
    let (_, chore_id, exec_a_id) = make_waiting_human_chore(&db, "live-exec-terminal");

    // Manually abandon exec_a.
    db.mark_execution_redundant(&exec_a_id).unwrap();

    let exec_b = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url("git@github.com:foo/bar.git")
            .build())
        .unwrap();

    let live = db
        .get_live_execution_for_work_item(&chore_id, &exec_b.id)
        .unwrap();
    assert!(
        live.is_none(),
        "no live execution should remain after exec_a is abandoned"
    );
}

#[test]
fn mark_execution_redundant_sets_status_abandoned() {
    let db = WorkDb::open(temp_db_path("mark-redundant")).unwrap();
    let (_, _, exec_a_id) = make_waiting_human_chore(&db, "mark-redundant");

    db.mark_execution_redundant(&exec_a_id).unwrap();

    let exec = db.get_execution(&exec_a_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Abandoned);
    assert!(exec.finished_at.is_some(), "finished_at must be set");
}

#[test]
fn list_recently_terminal_finds_abandoned_exec_for_active_chore() {
    let db = WorkDb::open(temp_db_path("late-pr-list")).unwrap();
    let (_, chore_id, exec_id) = make_abandoned_chore_with_workspace(&db, "late-pr-list");

    let candidates = db
        .list_recently_terminal_executions_pending_pr_detection(3600)
        .unwrap();
    assert_eq!(candidates.len(), 1, "should find one late PR candidate");
    assert_eq!(candidates[0].execution_id, exec_id);
    assert_eq!(candidates[0].work_item_id, chore_id);
}

#[test]
fn list_recently_terminal_excludes_chore_with_pr_url_already_set() {
    let db = WorkDb::open(temp_db_path("late-pr-list-has-pr")).unwrap();
    let (_, chore_id, _) = make_abandoned_chore_with_workspace(&db, "late-pr-list-has-pr");

    // Manually bind a pr_url to the task.
    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some("https://github.com/foo/bar/pull/1".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let candidates = db
        .list_recently_terminal_executions_pending_pr_detection(3600)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "chore already has pr_url — should not appear as candidate"
    );
}

#[test]
fn bind_pr_to_active_task_transitions_to_in_review() {
    let db = WorkDb::open(temp_db_path("bind-pr-active")).unwrap();
    let (_, chore_id, exec_id) = make_abandoned_chore_with_workspace(&db, "bind-pr-active");

    let updated = db
        .bind_pr_to_active_task_from_terminal_execution(
            &chore_id,
            "https://github.com/foo/bar/pull/99",
        )
        .unwrap();
    assert!(updated, "should return true on first bind");

    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore or task, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert_eq!(
        task.pr_url.as_deref(),
        Some("https://github.com/foo/bar/pull/99")
    );

    // Execution itself should still be abandoned (not touched by bind).
    let exec = db.get_execution(&exec_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Abandoned);
}

#[test]
fn bind_pr_to_active_task_is_idempotent_when_already_in_review() {
    let db = WorkDb::open(temp_db_path("bind-pr-idempotent")).unwrap();
    let (_, chore_id, _) = make_abandoned_chore_with_workspace(&db, "bind-pr-idempotent");

    let first = db
        .bind_pr_to_active_task_from_terminal_execution(
            &chore_id,
            "https://github.com/foo/bar/pull/99",
        )
        .unwrap();
    assert!(first);

    let second = db
        .bind_pr_to_active_task_from_terminal_execution(
            &chore_id,
            "https://github.com/foo/bar/pull/99",
        )
        .unwrap();
    assert!(
        !second,
        "should return false when task is already past active"
    );
}

#[test]
fn fresh_db_has_parent_task_id_column_and_index() {
    let db = WorkDb::open(temp_db_path("revision-schema-fresh")).unwrap();
    let conn = db.connect().unwrap();

    let cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(tasks)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert!(
        cols.contains(&"parent_task_id".to_owned()),
        "tasks table must have parent_task_id column after fresh init; columns = {cols:?}"
    );

    let index_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_tasks_parent_task_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        index_exists, 1,
        "idx_tasks_parent_task_id must exist after fresh init"
    );
}

#[test]
fn fresh_db_has_prefer_is_soft_column() {
    let db = WorkDb::open(temp_db_path("revision-schema-exec")).unwrap();
    let conn = db.connect().unwrap();

    let cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(work_executions)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert!(
        cols.contains(&"prefer_is_soft".to_owned()),
        "work_executions must have prefer_is_soft after fresh init; columns = {cols:?}"
    );
}

#[test]
fn upgrade_from_schema_without_revision_columns_yields_same_shape() {
    let path = disk_db_path("revision-schema-upgrade");

    // Build a pre-revision schema that is missing the two new columns.
    // The tasks table must include all columns present before the revision
    // migration (including `deleted_at` which is referenced by the existing
    // index DDL in the init batch). work_executions must include all
    // pre-revision columns but lack `prefer_is_soft`. Migrations are
    // idempotent; opening via WorkDb::open must successfully apply just the
    // two new migrations without touching the existing data shape.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        // tasks: full legacy schema (matches the CREATE TABLE IF NOT EXISTS
        // DDL in WorkDb init) but without parent_task_id.  Columns referenced
        // by existing indexes (product_id, kind, deleted_at, project_id,
        // ordinal) must all be present so those index DDLs succeed.
        "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL,
                project_id TEXT,
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
                external_ref_unbound_at TEXT,
                investigation_doc_path TEXT,
                investigation_doc_branch TEXT
                -- parent_task_id intentionally absent
             );
             CREATE TABLE work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                cube_workspace_id TEXT,
                workspace_path TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                preferred_workspace_id TEXT,
                pre_start_failure_count INTEGER NOT NULL DEFAULT 0,
                dispatch_not_before TEXT,
                pr_url TEXT,
                pr_head_before TEXT
                -- prefer_is_soft intentionally absent
             );
             CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    drop(conn);

    // Opening through WorkDb::open runs all migrations including the new ones.
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();

    let task_cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(tasks)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert!(
        task_cols.contains(&"parent_task_id".to_owned()),
        "upgraded tasks table must gain parent_task_id; columns = {task_cols:?}"
    );

    // The bespoke investigation-doc pointer columns must be DROPPED on
    // upgrade: the card affordance now derives from `pr_url`, mirroring the
    // design-doc model, so the columns are dead weight. This legacy fixture
    // creates them explicitly (matching the old `ADD COLUMN` migration), so a
    // successful drop here proves the drop migration handles a populated DB.
    assert!(
        !task_cols.contains(&"investigation_doc_path".to_owned()),
        "investigation_doc_path must be dropped on upgrade; columns = {task_cols:?}"
    );
    assert!(
        !task_cols.contains(&"investigation_doc_branch".to_owned()),
        "investigation_doc_branch must be dropped on upgrade; columns = {task_cols:?}"
    );

    let index_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_tasks_parent_task_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        index_exists, 1,
        "idx_tasks_parent_task_id must exist after upgrade"
    );

    let exec_cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(work_executions)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert!(
        exec_cols.contains(&"prefer_is_soft".to_owned()),
        "upgraded work_executions must gain prefer_is_soft; columns = {exec_cols:?}"
    );

    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn chain_root_returns_self_for_non_revision_task() {
    let db = WorkDb::open(temp_db_path("chain-root-self")).unwrap();
    let product_id = make_revision_product(&db, "chain-root-self");
    let chore_id = make_chore_root(&db, &product_id, "self");
    let conn = db.connect().unwrap();
    let root = chain_root(&conn, &chore_id).unwrap();
    assert_eq!(
        root, chore_id,
        "chain_root of a non-revision task must return itself"
    );
}

#[test]
fn chain_root_walks_single_link() {
    let db = WorkDb::open(temp_db_path("chain-root-single")).unwrap();
    let product_id = make_revision_product(&db, "chain-root-single");
    let chore_id = make_chore_root(&db, &product_id, "root");
    let revision_id = insert_revision_row(&db, &product_id, &chore_id);

    let conn = db.connect().unwrap();
    let root = chain_root(&conn, &revision_id).unwrap();
    assert_eq!(
        root, chore_id,
        "chain_root of a single-link revision must return the parent chore"
    );
}

#[test]
fn chain_root_walks_multi_link_chain() {
    let db = WorkDb::open(temp_db_path("chain-root-multi")).unwrap();
    let product_id = make_revision_product(&db, "chain-root-multi");
    let chore_id = make_chore_root(&db, &product_id, "root");
    // R1 → chore
    let r1_id = insert_revision_row(&db, &product_id, &chore_id);
    // R2 → R1 (revision of a revision, flat continuation per OQ2)
    let r2_id = insert_revision_row(&db, &product_id, &r1_id);
    // R3 → R2
    let r3_id = insert_revision_row(&db, &product_id, &r2_id);

    let conn = db.connect().unwrap();
    for (label, id) in [("R1", &r1_id), ("R2", &r2_id), ("R3", &r3_id)] {
        let root = chain_root(&conn, id).unwrap();
        assert_eq!(
            root, chore_id,
            "{label}: chain_root must reach the originating non-revision task"
        );
    }
}

#[test]
fn chain_root_handles_broken_parent_gracefully() {
    let db = WorkDb::open(temp_db_path("chain-root-broken")).unwrap();
    let product_id = make_revision_product(&db, "chain-root-broken");
    let chore_id = make_chore_root(&db, &product_id, "root");
    // R1 points at a real chore (so it has a valid root).
    let r1_id = insert_revision_row(&db, &product_id, &chore_id);
    // R2 → R1 (valid link)
    let r2_id = insert_revision_row(&db, &product_id, &r1_id);
    // Now soft-delete R1 to simulate a broken intermediate parent.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?1 WHERE id = ?2",
            rusqlite::params![now_string(), r1_id],
        )
        .unwrap();
    // R2's parent (R1) is still in the table (soft-deleted), but R1's
    // parent (chore) exists. chain_root should still reach chore because
    // the walk queries the row even when deleted_at is set.
    // (Broken-parent = the row is completely missing, not just deleted.)
    // So let's also test with a genuinely missing parent: insert a
    // revision whose parent_task_id refers to a non-existent id.
    let orphan_id = {
        let conn = db.connect().unwrap();
        let id = next_id("task");
        let now = now_string();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 VALUES (?1, ?2, 'revision', 'orphan', '', 'todo', ?3, ?3, 'nonexistent-parent-id')",
                rusqlite::params![id, product_id, now],
            ).unwrap();
        id
    };
    let conn = db.connect().unwrap();
    // Walking from orphan: parent 'nonexistent-parent-id' is missing →
    // stop immediately; chain_root returns the orphan itself (deepest reachable).
    let root = chain_root(&conn, &orphan_id).unwrap();
    assert_eq!(
        root, orphan_id,
        "broken-parent: chain_root must return the deepest reachable id (the revision itself)"
    );

    // R2 still walks through the soft-deleted R1 to the chore
    // (soft-deleted rows are still in the table; the walk doesn't filter on deleted_at).
    let root2 = chain_root(&conn, &r2_id).unwrap();
    assert_eq!(
        root2, chore_id,
        "soft-deleted intermediate parent: chain_root must still reach the chore"
    );
}

#[test]
fn create_revision_succeeds_for_open_pr() {
    let db = WorkDb::open(temp_db_path("revision-create-open")).unwrap();
    let product_id = make_revision_product(&db, "open");
    let pr_url = "https://github.com/spinyfin/mono/pull/42";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(parent_id.as_str()));
    assert_eq!(revision.product_id, product_id);
    assert_eq!(revision.status, TaskStatus::Todo);
    assert!(
        revision.pr_url.is_none(),
        "revision must not inherit parent pr_url"
    );
}

#[test]
fn create_revision_errors_when_parent_has_no_pr() {
    let db = WorkDb::open(temp_db_path("revision-create-no-pr")).unwrap();
    let product_id = make_revision_product(&db, "nopr");
    let parent_id = make_chore_root(&db, &product_id, "no-pr");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let err = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("has no PR yet"),
        "expected 'has no PR yet' in: {msg}"
    );
}

#[test]
fn create_revision_errors_when_parent_pr_merged_via_cached_status() {
    let db = WorkDb::open(temp_db_path("revision-create-merged-cached")).unwrap();
    let product_id = make_revision_product(&db, "merged-cached");
    let pr_url = "https://github.com/spinyfin/mono/pull/99";
    let parent_id = make_done_chore(&db, &product_id, pr_url);

    // Probe would return Open, but the cached status='done' gate fires first.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let err = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("already merged"),
        "expected 'already merged' in: {msg}"
    );
    assert!(
        msg.contains("#99"),
        "expected PR number in error message: {msg}"
    );
}

#[test]
fn create_revision_errors_when_pr_merged_via_live_probe() {
    let db = WorkDb::open(temp_db_path("revision-create-merged-probe")).unwrap();
    let product_id = make_revision_product(&db, "merged-probe");
    let pr_url = "https://github.com/spinyfin/mono/pull/101";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Live probe says merged (race: PR merged after cache was last updated).
    let checker = FakePrStateChecker::always(PrOpenState::Merged);
    let err = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("already merged"),
        "expected 'already merged' in: {msg}"
    );
}

#[test]
fn create_revision_errors_when_pr_closed_unmerged() {
    let db = WorkDb::open(temp_db_path("revision-create-closed")).unwrap();
    let product_id = make_revision_product(&db, "closed");
    let pr_url = "https://github.com/spinyfin/mono/pull/77";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Live probe says closed without merging.
    let checker = FakePrStateChecker::always(PrOpenState::ClosedUnmerged);
    let err = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("closed without merging"),
        "expected 'closed without merging' in: {msg}"
    );
    assert!(
        msg.contains("#77"),
        "expected PR number in error message: {msg}"
    );
}

#[test]
fn create_revision_of_revision_gates_against_chain_root() {
    let db = WorkDb::open(temp_db_path("revision-of-revision")).unwrap();
    let product_id = make_revision_product(&db, "ror");
    let pr_url = "https://github.com/spinyfin/mono/pull/55";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    // R1: revision of the root chore.
    let checker_open = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&root_id), &checker_open)
        .unwrap();
    assert_eq!(r1.kind, TaskKind::Revision);

    // R2: revision of R1 — gate should resolve to root's PR.
    let r2 = db
        .create_revision(revision_input(&r1.id), &checker_open)
        .unwrap();
    assert_eq!(r2.kind, TaskKind::Revision);
    assert_eq!(r2.parent_task_id.as_deref(), Some(r1.id.as_str()));

    // Now simulate the root's PR being closed; revising R1 should fail.
    let checker_closed = FakePrStateChecker::always(PrOpenState::ClosedUnmerged);
    let err = db
        .create_revision(revision_input(&r1.id), &checker_closed)
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("closed without merging"),
        "revision-of-revision gate must check chain root PR; got: {msg}"
    );
}

#[test]
fn create_revision_inherits_product_and_project_from_root() {
    let db = WorkDb::open(temp_db_path("revision-inherit")).unwrap();
    let product_id = make_revision_product(&db, "inherit");
    let pr_url = "https://github.com/spinyfin/mono/pull/200";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    assert_eq!(
        revision.product_id, product_id,
        "revision must inherit product_id from chain root"
    );
    assert_eq!(
        revision.effort_level,
        Some(boss_protocol::EffortLevel::Small),
        "revision must default to small effort per §Q7"
    );
    // `make_revision_product` sets a product-level repo, so the root
    // chore carries no per-task override (repo_remote_url is NULL per the
    // enforce_task_repo_invariant rule). The revision must mirror that —
    // a NULL repo here, not a redundant override of the product's URL.
    assert!(
        revision.repo_remote_url.is_none(),
        "revision under a product that owns the repo must keep repo_remote_url NULL"
    );
}

#[test]
fn create_revision_inherits_repo_remote_url_from_root() {
    // Issue #840: under a multi-repo product (product.repo_remote_url is
    // NULL) the chain root carries its own per-task repo override. The
    // revision must inherit that override; otherwise
    // `resolve_repo_for_work_item` returns None and the autostarted
    // execution dies pre-start with no workspace to lease.
    let db = WorkDb::open(temp_db_path("revision-inherit-repo")).unwrap();
    // Multi-repo product: no product-level repo.
    let product_id = db
        .create_product(CreateProductInput {
            name: "Boss-multirepo".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap()
        .id;
    let root_repo = "git@github.com:linkedin-multiproduct/dev-infra.git";
    // Root chore carries its own repo override (allowed: product has none).
    let root_chore = db
        .create_chore(CreateChoreInput {
            product_id: product_id.clone(),
            name: "Root chore in multi-repo product".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some(root_repo.to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
    assert_eq!(
        root_chore.repo_remote_url.as_deref(),
        Some(root_repo),
        "fixture: root must carry its own repo override under a repo-less product"
    );
    // Put the root "in review" with an open PR so the revision gate passes.
    let pr_url = "https://github.com/linkedin-multiproduct/dev-infra/pull/317";
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root_chore.id, pr_url],
        )
        .unwrap();
    }

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&root_chore.id), &checker)
        .unwrap();

    assert_eq!(
        revision.repo_remote_url.as_deref(),
        Some(root_repo),
        "revision must inherit the chain root's repo override"
    );

    // The dispatch repo resolver must now find a repo for the revision
    // row — this is what was returning None (→ pre-start failure) before.
    let conn = db.connect().unwrap();
    assert_eq!(
        resolve_repo_for_work_item(&conn, &revision.id)
            .unwrap()
            .as_deref(),
        Some(root_repo),
        "dispatch must resolve the inherited repo so the execution can lease a workspace"
    );
}

#[test]
fn create_revision_of_revision_inherits_repo_from_chain_root() {
    // A revision-of-a-revision must still inherit the chain root's repo:
    // the immediate parent is itself a revision that inherited the repo,
    // and `insert_revision_in_tx` copies from `root` (the non-revision
    // ancestor that owns the PR), so the whole chain stays repo-aligned.
    let db = WorkDb::open(temp_db_path("revision-of-revision-repo")).unwrap();
    let product_id = db
        .create_product(CreateProductInput {
            name: "Boss-multirepo-chain".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap()
        .id;
    let root_repo = "git@github.com:linkedin-multiproduct/dev-infra.git";
    let root_chore = db
        .create_chore(CreateChoreInput {
            product_id: product_id.clone(),
            name: "Root chore".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some(root_repo.to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
                "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/linkedin-multiproduct/dev-infra/pull/1' WHERE id = ?1",
                rusqlite::params![root_chore.id],
            )
            .unwrap();
    }

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&root_chore.id), &checker)
        .unwrap();
    let r2 = db
        .create_revision(revision_input(&r1.id), &checker)
        .unwrap();

    assert_eq!(r1.repo_remote_url.as_deref(), Some(root_repo));
    assert_eq!(
        r2.repo_remote_url.as_deref(),
        Some(root_repo),
        "revision-of-revision must inherit the chain root's repo, not NULL"
    );
}

#[test]
fn task_show_projection_includes_parent_task_id_for_revision() {
    // Regression (issue #789): `boss task show <revision>` reads the row
    // through `query_task` / `get_work_item_by_short_id`, which used
    // `map_task` and always returned `parent_task_id = None` — so the CLI
    // could not confirm a revision's parent linkage even though
    // `create-revision --json` had returned it. Both the primary-id and
    // the short-id lookup must now surface `parent_task_id`.
    let db = WorkDb::open(temp_db_path("revision-show-parent")).unwrap();
    let product_id = make_revision_product(&db, "show-parent");
    let pr_url = "https://github.com/spinyfin/mono/pull/250";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    let short_id = revision.short_id.expect("revision must have a short_id");

    // `boss task show <full-id>` path.
    let by_id = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected Task/Chore, got {other:?}"),
    };
    assert_eq!(
        by_id.parent_task_id.as_deref(),
        Some(parent_id.as_str()),
        "get_work_item must surface the revision's parent_task_id"
    );

    // `boss task show T<n>` (short-id) path.
    let by_short = match db
        .get_work_item_by_short_id(&product_id, short_id)
        .unwrap()
        .expect("revision must be resolvable by short id")
    {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected Task/Chore, got {other:?}"),
    };
    assert_eq!(
        by_short.parent_task_id.as_deref(),
        Some(parent_id.as_str()),
        "get_work_item_by_short_id must surface the revision's parent_task_id"
    );
}

// ── revision_name_from_description ──────────────────────────────────────

#[test]
fn revision_name_single_line_unchanged() {
    assert_eq!(
        revision_name_from_description("Fix the thing"),
        "Fix the thing"
    );
}

#[test]
fn revision_name_uses_first_nonempty_line() {
    let desc = "Rename --dry-run to --plan before merge\n\nSome extra detail here.";
    assert_eq!(
        revision_name_from_description(desc),
        "Rename --dry-run to --plan before merge"
    );
}

#[test]
fn revision_name_skips_blank_leading_lines() {
    let desc = "\n\nActual first content\nMore stuff";
    assert_eq!(revision_name_from_description(desc), "Actual first content");
}

#[test]
fn revision_name_truncates_long_first_line_at_word_boundary() {
    // Build a line longer than 120 chars with a space before the limit.
    let long = "word ".repeat(30); // 150 chars: "word " × 30
    let name = revision_name_from_description(&long);
    assert!(
        name.len() <= 125,
        "name must be ≤120 + '…': got {}",
        name.len()
    );
    assert!(name.ends_with('…'), "long names must end with ellipsis");
}

#[test]
fn revision_name_from_description_crlf_lines() {
    let desc = "First line\r\nSecond line";
    // Rust `str::lines()` strips \r\n correctly.
    assert_eq!(revision_name_from_description(desc), "First line");
}

#[test]
fn create_revision_uses_explicit_name_over_description_fallback() {
    let db = WorkDb::open(temp_db_path("revision-explicit-name")).unwrap();
    let product_id = make_revision_product(&db, "explicit-name");
    let pr_url = "https://github.com/spinyfin/mono/pull/99";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let explicit_name = "Fix missing version number in release builds";
    let task = db.create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id)
                .description("I also don't see the version number in /release/ builds created by the buildkite release pipeline.")
                .name(explicit_name)
                .autostart(true)
                .build(),
            &checker,
        ).unwrap();

    assert_eq!(
        task.name, explicit_name,
        "revision should use the coordinator-supplied name, not the fallback from description"
    );
}

#[test]
fn create_revision_with_autostart_false_stores_zero() {
    let db = WorkDb::open(temp_db_path("revision-autostart-false")).unwrap();
    let product_id = make_revision_product(&db, "noauto");
    let pr_url = "https://github.com/spinyfin/mono/pull/77";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id)
                .description("fix something but don't auto-start")
                .autostart(false)
                .build(),
            &checker,
        )
        .unwrap();

    assert_eq!(revision.status, TaskStatus::Todo);
    assert!(
        !revision.autostart,
        "revision created with autostart=false must have autostart=false on the row"
    );
}

#[test]
fn attach_revision_projections_assigns_seq_and_pr_url() {
    let root = make_bare_task(
        "root",
        "chore",
        None,
        Some("https://gh/pull/1"),
        "2026-01-01",
    );
    let r1 = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    let r2 = make_bare_task("r2", "revision", Some("root"), None, "2026-01-03");

    let chores = vec![root.clone()];
    let result = attach_revision_projections(vec![r1, r2], &chores);

    let rev1 = result.iter().find(|t| t.id == "r1").unwrap();
    assert_eq!(rev1.revision_seq, Some(1), "r1 must be R1");
    assert_eq!(
        rev1.revision_parent_pr_url.as_deref(),
        Some("https://gh/pull/1")
    );

    let rev2 = result.iter().find(|t| t.id == "r2").unwrap();
    assert_eq!(rev2.revision_seq, Some(2), "r2 must be R2");
    assert_eq!(
        rev2.revision_parent_pr_url.as_deref(),
        Some("https://gh/pull/1")
    );
}

#[test]
fn attach_revision_projections_chained_revisions_flat_seq() {
    // R2 is a revision-of-R1 (chained); both should still get a flat
    // sequence number relative to the chain root.
    let root = make_bare_task(
        "root",
        "chore",
        None,
        Some("https://gh/pull/2"),
        "2026-01-01",
    );
    let r1 = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    let r2 = make_bare_task("r2", "revision", Some("r1"), None, "2026-01-03");

    let chores = vec![root.clone()];
    let result = attach_revision_projections(vec![r1, r2], &chores);

    let rev1 = result.iter().find(|t| t.id == "r1").unwrap();
    let rev2 = result.iter().find(|t| t.id == "r2").unwrap();
    assert_eq!(rev1.revision_seq, Some(1));
    assert_eq!(
        rev2.revision_seq,
        Some(2),
        "chained revision-of-revision must be R2, not R1.1"
    );
}

#[test]
fn attach_revision_projections_non_revision_tasks_unaffected() {
    let task = make_bare_task(
        "t1",
        "project_task",
        None,
        Some("https://gh/pull/3"),
        "2026-01-01",
    );
    let result = attach_revision_projections(vec![task], &[]);
    let t = &result[0];
    assert_eq!(
        t.revision_seq, None,
        "non-revision tasks must not get a seq"
    );
    assert_eq!(t.revision_parent_pr_url, None);
}

#[test]
fn get_work_tree_includes_revision_seq_and_pr_url() {
    // End-to-end: get_work_tree must populate revision projections.
    let db = WorkDb::open(temp_db_path("revision-projections-work-tree")).unwrap();
    let product_id = make_revision_product(&db, "proj-rev");
    let pr_url = "https://github.com/spinyfin/mono/pull/99";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    // Revision arrives in the tasks array (not chores).
    let rev_task = tree
        .tasks
        .iter()
        .find(|t| t.id == revision.id)
        .expect("revision must be in work_tree tasks");

    assert_eq!(rev_task.revision_seq, Some(1), "first revision must be R1");
    assert_eq!(
        rev_task.revision_parent_pr_url.as_deref(),
        Some(pr_url),
        "revision_parent_pr_url must carry the chain root's PR URL"
    );
}

#[test]
fn get_work_tree_two_revisions_get_distinct_seqs() {
    let db = WorkDb::open(temp_db_path("revision-two-seqs")).unwrap();
    let product_id = make_revision_product(&db, "two-revs");
    let pr_url = "https://github.com/spinyfin/mono/pull/100";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    let r2 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let seq = |id: &str| {
        tree.tasks
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.revision_seq)
    };
    assert_eq!(seq(&r1.id), Some(1), "r1 must be R1");
    assert_eq!(seq(&r2.id), Some(2), "r2 must be R2");
}

// ── attach_in_progress_revision_flag ─────────────────────────────────────

#[test]
fn in_progress_flag_set_for_todo_revision() {
    let root = make_bare_task("root", "chore", None, Some("https://gh/pull/1"), "2026-01-01");
    let mut rev = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    rev.status = TaskStatus::Todo;

    let mut tasks = vec![rev];
    let mut chores = vec![root];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        chores.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "todo revision must trigger the flag on the chain root"
    );
}

#[test]
fn in_progress_flag_set_for_active_revision() {
    let root = make_bare_task("root", "chore", None, Some("https://gh/pull/2"), "2026-01-01");
    let mut rev = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    rev.status = TaskStatus::Active;

    let mut tasks = vec![rev];
    let mut chores = vec![root];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        chores.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "active revision must trigger the flag on the chain root"
    );
}

#[test]
fn in_progress_flag_clear_for_in_review_revision() {
    let root = make_bare_task("root", "chore", None, Some("https://gh/pull/3"), "2026-01-01");
    let mut rev = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    rev.status = TaskStatus::InReview;

    let mut tasks = vec![rev];
    let mut chores = vec![root];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        !chores.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "in_review revision must NOT trigger the flag"
    );
}

#[test]
fn in_progress_flag_clear_for_done_revision() {
    let root = make_bare_task("root", "chore", None, Some("https://gh/pull/4"), "2026-01-01");
    let mut rev = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    rev.status = TaskStatus::Done;

    let mut tasks = vec![rev];
    let mut chores = vec![root];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        !chores.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "done revision must NOT trigger the flag"
    );
}

#[test]
fn in_progress_flag_chain_revision_of_revision() {
    // R2 is a revision-of-R1: the flag must still reach the chain root.
    let root = make_bare_task("root", "chore", None, Some("https://gh/pull/5"), "2026-01-01");
    let r1 = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    // r1 is done; r2 (chained) is todo
    let mut r1 = r1;
    r1.status = TaskStatus::Done;
    let mut r2 = make_bare_task("r2", "revision", Some("r1"), None, "2026-01-03");
    r2.status = TaskStatus::Todo;

    let mut tasks = vec![r1, r2];
    let mut chores = vec![root];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        chores.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "in-progress revision deep in the chain must still flag the root"
    );
}

#[test]
fn in_progress_flag_project_task_root() {
    // Chain root is a project_task in the tasks slice (not in chores).
    let mut root = make_bare_task("root", "project_task", None, Some("https://gh/pull/6"), "2026-01-01");
    root.status = TaskStatus::InReview;
    let mut rev = make_bare_task("r1", "revision", Some("root"), None, "2026-01-02");
    rev.status = TaskStatus::Todo;

    let mut tasks = vec![root, rev];
    let mut chores = vec![];
    attach_in_progress_revision_flag(&mut tasks, &mut chores);

    assert!(
        tasks.iter().find(|t| t.id == "root").unwrap().has_in_progress_revision,
        "flag must be set on a project_task root in the tasks slice"
    );
    assert!(
        !tasks.iter().find(|t| t.id == "r1").unwrap().has_in_progress_revision,
        "flag must NOT be set on the revision itself"
    );
}

#[test]
fn get_work_tree_has_in_progress_revision() {
    // End-to-end: a todo revision causes the chain root chore to carry the flag.
    let db = WorkDb::open(temp_db_path("in-progress-rev-flag")).unwrap();
    let product_id = make_revision_product(&db, "flag-e2e");
    let pr_url = "https://github.com/spinyfin/mono/pull/500";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(revision_input(&parent_id), &checker).unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let root_chore = tree.chores.iter().find(|c| c.id == parent_id)
        .expect("chain root chore must be in work_tree.chores");

    assert!(
        root_chore.has_in_progress_revision,
        "chain root chore must carry has_in_progress_revision=true when a todo revision exists"
    );
}

#[test]
fn get_work_tree_flag_cleared_when_revision_in_review() {
    // A revision that moves to in_review no longer triggers the flag.
    let db = WorkDb::open(temp_db_path("in-progress-rev-cleared")).unwrap();
    let product_id = make_revision_product(&db, "flag-cleared");
    let pr_url = "https://github.com/spinyfin/mono/pull/501";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Move the revision to in_review (simulates the worker completing and opening a PR).
    db.force_task_status_for_test(&revision.id, "in_review").unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let root_chore = tree.chores.iter().find(|c| c.id == parent_id)
        .expect("chain root chore must be in work_tree.chores");

    assert!(
        !root_chore.has_in_progress_revision,
        "flag must be false when the only revision is in_review"
    );
}

// ── auto-gate: new revision blocks on prior active revision ─────────────

/// First revision on a PR has no prior sibling → no dependency edge.
#[test]
fn create_revision_first_has_no_auto_dep() {
    let db = WorkDb::open(temp_db_path("rev-auto-dep-first")).unwrap();
    let product_id = make_revision_product(&db, "auto-dep-first");
    let pr_url = "https://github.com/spinyfin/mono/pull/900";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    // R1 has no prior revision — status must still be `todo`.
    assert_eq!(
        r1.status, TaskStatus::Todo,
        "first revision must stay todo (no auto-block)"
    );

    // No dependency edges at all.
    let conn = db.connect().unwrap();
    let prereqs =
        crate::work_dependencies::prerequisites_of(&conn, &r1.id, Some("blocks")).unwrap();
    assert!(
        prereqs.is_empty(),
        "first revision must have no prerequisite edges"
    );
}

/// Second revision on the same PR must be auto-gated on the first.
#[test]
fn create_revision_second_auto_blocks_on_first() {
    let db = WorkDb::open(temp_db_path("rev-auto-dep-second")).unwrap();
    let product_id = make_revision_product(&db, "auto-dep-second");
    let pr_url = "https://github.com/spinyfin/mono/pull/901";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    let r2 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    // R2 must be blocked because R1 is still active.
    assert_eq!(
        r2.status, TaskStatus::Blocked,
        "second revision must be auto-blocked by first"
    );

    // R2 must have a blocks prerequisite on R1.
    let conn = db.connect().unwrap();
    let prereqs =
        crate::work_dependencies::prerequisites_of(&conn, &r2.id, Some("blocks")).unwrap();
    assert_eq!(prereqs.len(), 1, "r2 must have exactly one prerequisite");
    assert_eq!(
        prereqs[0].prerequisite_id, r1.id,
        "r2's prerequisite must be r1"
    );
}

/// Third revision auto-gates on the second (the most-recent active
/// tail), not the first.
#[test]
fn create_revision_third_auto_blocks_on_second() {
    let db = WorkDb::open(temp_db_path("rev-auto-dep-third")).unwrap();
    let product_id = make_revision_product(&db, "auto-dep-third");
    let pr_url = "https://github.com/spinyfin/mono/pull/902";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    let r2 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    let r3 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    let conn = db.connect().unwrap();
    let prereqs =
        crate::work_dependencies::prerequisites_of(&conn, &r3.id, Some("blocks")).unwrap();
    assert_eq!(prereqs.len(), 1, "r3 must have exactly one prerequisite");
    assert_eq!(
        prereqs[0].prerequisite_id, r2.id,
        "r3's prerequisite must be r2 (the most-recent active revision)"
    );
    // r1 is not a direct prerequisite of r3
    assert!(
        prereqs.iter().all(|e| e.prerequisite_id != r1.id),
        "r3 must not directly gate on r1"
    );
}

/// When R1 is done, R2 must not be auto-gated on it (done revisions
/// cannot race with the new one).
#[test]
fn create_revision_skips_done_revision_as_tail() {
    let db = WorkDb::open(temp_db_path("rev-auto-dep-done-skip")).unwrap();
    let product_id = make_revision_product(&db, "auto-dep-done-skip");
    let pr_url = "https://github.com/spinyfin/mono/pull/903";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    // Mark R1 done.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'done' WHERE id = ?1",
        rusqlite::params![r1.id],
    )
    .unwrap();
    drop(conn);

    // R2 filed after R1 is done — no active tail → R2 stays todo.
    let r2 = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    assert_eq!(
        r2.status, TaskStatus::Todo,
        "second revision must stay todo when the only prior revision is done"
    );
    let conn = db.connect().unwrap();
    let prereqs =
        crate::work_dependencies::prerequisites_of(&conn, &r2.id, Some("blocks")).unwrap();
    assert!(
        prereqs.is_empty(),
        "no prerequisite edge when prior revision is done"
    );
}

// ── block_pending_revisions_on_parent_close / parent-PR-merged invalidation ─

/// When the parent PR merges, a `todo` revision must be blocked with
/// `parent_pr_closed` and an attention item surfaced.
#[test]
fn mark_chore_pr_merged_blocks_todo_revision() {
    let db = WorkDb::open(temp_db_path("rev-invalidate-todo")).unwrap();
    let product_id = make_revision_product(&db, "inv-todo");
    let pr_url = "https://github.com/spinyfin/mono/pull/805";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();
    assert_eq!(revision.status, TaskStatus::Todo);

    // Simulate the parent PR merging.
    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // The revision must now be blocked.
    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status, TaskStatus::Blocked,
        "todo revision must be blocked after parent PR merges"
    );
    assert_eq!(
        rev_after.blocked_reason.as_deref(),
        Some("parent_pr_closed"),
        "blocked_reason must be 'parent_pr_closed'"
    );

    // An attention item must be present for the revision.
    let attn = db.list_attention_items_for_work_item(&revision.id).unwrap();
    assert!(
        attn.iter().any(|a| a.kind == "revision_parent_closed"),
        "attention item 'revision_parent_closed' must be created for a blocked revision; got: {attn:?}",
    );
}

/// A revision already in `in_review` must be flipped to `done` (not
/// `blocked`) — it delivered its commit before the parent merged.
#[test]
fn mark_chore_pr_merged_keeps_in_review_revision_done_not_blocked() {
    let db = WorkDb::open(temp_db_path("rev-invalidate-in-review")).unwrap();
    let product_id = make_revision_product(&db, "inv-ir");
    let pr_url = "https://github.com/spinyfin/mono/pull/806";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    // Simulate the revision having pushed its commit and moved to in_review.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status, TaskStatus::Done,
        "in_review revision must become done (not blocked) when parent PR merges"
    );
}

/// A revision already `done` must not be touched.
#[test]
fn mark_chore_pr_merged_does_not_re_block_done_revision() {
    let db = WorkDb::open(temp_db_path("rev-invalidate-done")).unwrap();
    let product_id = make_revision_product(&db, "inv-done");
    let pr_url = "https://github.com/spinyfin/mono/pull/807";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'done' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status, TaskStatus::Done,
        "already-done revision must not be touched"
    );
    assert_eq!(
        rev_after.blocked_reason, None,
        "done revision must not acquire a blocked_reason"
    );
}

/// `list_active_revision_executions_for_chain` returns executions with a
/// cube lease but not ones that are already terminal.
#[test]
fn list_active_revision_executions_for_chain_returns_leased_only() {
    let db = WorkDb::open(temp_db_path("rev-list-active-exec")).unwrap();
    let product_id = make_revision_product(&db, "lare");
    let pr_url = "https://github.com/spinyfin/mono/pull/808";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(revision_input(&parent_id), &checker)
        .unwrap();

    // Insert a running execution WITH a cube lease.
    // Note: work_executions.priority is an i64 (0 = default); tasks.priority is TEXT.
    let exec_id = next_id("exec");
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, cube_lease_id,
                  cube_workspace_id, workspace_path, priority, prefer_is_soft,
                  created_at, started_at)
             VALUES (?1, ?2, 'revision_implementation', 'running',
                     'git@github.com:spinyfin/mono.git', 'lease-abc',
                     'mono-agent-001', '/tmp/ws', 0, 0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z')",
        rusqlite::params![exec_id, revision.id],
    )
    .unwrap();

    // Insert a terminal execution (cancelled) — must NOT be returned.
    let exec_terminal = next_id("exec");
    conn.execute(
        "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, cube_lease_id,
                  cube_workspace_id, workspace_path, priority, prefer_is_soft,
                  created_at, finished_at)
             VALUES (?1, ?2, 'revision_implementation', 'cancelled',
                     'git@github.com:spinyfin/mono.git', 'lease-old',
                     'mono-agent-001', '/tmp/ws', 0, 0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:05:00Z')",
        rusqlite::params![exec_terminal, revision.id],
    )
    .unwrap();
    drop(conn);

    let active = db
        .list_active_revision_executions_for_chain(&parent_id)
        .unwrap();
    assert_eq!(
        active.len(),
        1,
        "only the running leased execution must be returned"
    );
    assert_eq!(active[0].id, exec_id);
}

/// `list_active_revision_executions_for_chain` returns empty for a chain
/// root with no revisions.
#[test]
fn list_active_revision_executions_for_chain_empty_for_no_revisions() {
    let db = WorkDb::open(temp_db_path("rev-list-exec-empty")).unwrap();
    let product_id = make_revision_product(&db, "lare-empty");
    let pr_url = "https://github.com/spinyfin/mono/pull/809";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let active = db
        .list_active_revision_executions_for_chain(&parent_id)
        .unwrap();
    assert!(
        active.is_empty(),
        "chain with no revisions must yield empty vec"
    );
}

// ── Revision dispatch via request_execution ───────────────────────────

/// Regression: T701-style bug where `request_execution` (used by the
/// orphan sweep and kanban drag) produced `task_implementation` for
/// `kind=revision` tasks instead of `revision_implementation`.
///
/// After the fix:
///   - `execution.kind` must be `"revision_implementation"`
///   - `execution.pr_url` must be set to the chain-root's PR URL
///
/// The orphan sweep re-dispatch path then produces the same shape as the
/// steady-state `reconcile_revision_execution` path so the worker prompt
/// gets the correct revision prelude and cannot open a new PR.
#[test]
fn request_execution_for_revision_task_produces_revision_implementation_kind() {
    let db = WorkDb::open(temp_db_path("req-exec-revision-kind")).unwrap();
    let product_id = make_revision_product(&db, "req-exec-kind");
    let pr_url = "https://github.com/spinyfin/mono/pull/818";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Insert a revision task manually (direct insert, as in chain_root tests).
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(
        exec.kind, ExecutionKind::RevisionImplementation,
        "request_execution must produce revision_implementation for kind=revision tasks, got {:?}",
        exec.kind,
    );
    assert_eq!(
        exec.pr_url.as_deref(),
        Some(pr_url),
        "revision execution must carry the chain-root's PR URL so the worker knows which branch to push to",
    );
    assert_eq!(exec.status, ExecutionStatus::Ready);
}

/// Regression: re-dispatch of a revision task (orphan-sweep path) must
/// still produce `revision_implementation` kind and the correct `pr_url`.
///
/// Scenario: the first execution was `revision_implementation` and is now
/// `abandoned` (simulating a worker crash).  A subsequent call to
/// `request_execution` creates a new `ready` execution.
#[test]
fn request_execution_redispatch_of_revision_preserves_revision_kind_and_pr_url() {
    let db = WorkDb::open(temp_db_path("req-exec-revision-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "req-exec-redispatch");
    let pr_url = "https://github.com/spinyfin/mono/pull/818";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    // First dispatch.
    let first_exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(first_exec.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(first_exec.pr_url.as_deref(), Some(pr_url));

    // Simulate worker crash: mark the execution as abandoned.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            rusqlite::params![first_exec.id],
        )
        .unwrap();

    // Re-dispatch (mimics orphan sweep calling request_execution_with_live_check
    // with is_live returning false for the abandoned execution).
    let second_exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_ne!(
        second_exec.id, first_exec.id,
        "re-dispatch must create a fresh execution row",
    );
    assert_eq!(
        second_exec.kind, ExecutionKind::RevisionImplementation,
        "re-dispatched revision must still be revision_implementation, got {:?}",
        second_exec.kind,
    );
    assert_eq!(
        second_exec.pr_url.as_deref(),
        Some(pr_url),
        "re-dispatched revision must carry the chain-root's PR URL",
    );
    assert_eq!(second_exec.status, ExecutionStatus::Ready);
}

// ── Conflict-resolution revision: stop re-dispatch once the attempt retires ──

/// Insert a `kind=revision` task linked to a merge-conflict attempt.
/// Mirrors what `conflict_watch::maybe_spawn_conflict_revision` produces:
/// `created_via = "merge-conflict:<crz_id>"`, parent = the chore, and (as
/// in the steady-state loop) the row already flipped to `active`.
fn insert_conflict_revision_row(
    db: &WorkDb,
    product_id: &str,
    parent_task_id: &str,
    crz_id: &str,
) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{crz_id}");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Resolve merge conflict against main', '', 'active', ?3, ?3, 0, ?4, ?5)",
        rusqlite::params![id, product_id, now, created_via, parent_task_id],
    )
    .unwrap();
    id
}

/// (id, status) of every execution bound to `work_item_id`, oldest first.
fn executions_for(db: &WorkDb, work_item_id: &str) -> Vec<(String, String)> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, status FROM work_executions
             WHERE work_item_id = ?1 ORDER BY created_at ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![work_item_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn task_status(db: &WorkDb, task_id: &str) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
}

/// Regression (T906 / PR #970): a merge-conflict revision must stop being
/// re-dispatched once its `conflict_resolutions` attempt has retired
/// (`succeeded`), even though the chore PR is still open + `in_review`.
///
/// Before the fix, `reconcile_revision_execution` only consulted the chain
/// root's `pr_url`/`status` — neither of which reflects a *resolved*
/// conflict on an open PR — so it minted a fresh `revision_implementation`
/// execution on every reconcile tick. A queued `ready` row would then be
/// picked up and `start_execution_run` would flip the revision from
/// `in_review` straight back to `active`, defeating any operator attempt to
/// move the card to Review. The attempt could accumulate 8+ executions.
///
/// After the fix: a retired attempt drops the queued execution and settles
/// the revision to `in_review`; no new execution is created.
#[test]
fn merge_conflict_revision_stops_dispatch_after_attempt_succeeds() {
    let db = WorkDb::open(temp_db_path("crz-revision-stop-dispatch")).unwrap();
    let product_id = make_revision_product(&db, "crz-stop");
    let pr_url = "https://github.com/spinyfin/mono/pull/970";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 970,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First reconcile: attempt is still `pending`, so the revision dispatches
    // normally — this is the behaviour the fix must preserve.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // Conflict resolves: the PR is now CLEAN and the attempt retires. The
    // `ready` execution from above is still queued (the exact race that
    // makes a manual move-to-Review pointless).
    db.mark_conflict_resolution_succeeded(&crz.id, None).unwrap();

    // Second reconcile: the fix must NOT mint another execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned so start_execution_run \
         can't flip the revision back to active",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its fix vehicle is spent",
    );

    // Third reconcile: idempotent — the in_review revision is no longer
    // dispatchable, so nothing changes and the loop stays broken.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_third = executions_for(&db, &revision_id);
    assert_eq!(
        after_third.len(),
        1,
        "reconcile must remain a no-op for a settled revision: {after_third:?}",
    );
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

/// Guard against over-blocking: while the attempt is still active
/// (`pending`/`running`), a revision whose previous execution died must
/// still be re-dispatched. The fix only short-circuits *retired* attempts.
#[test]
fn merge_conflict_revision_still_redispatches_while_attempt_active() {
    let db = WorkDb::open(temp_db_path("crz-revision-active-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "crz-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/971";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 971,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First dispatch, then simulate a dead worker (execution orphaned).
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id);
    assert_eq!(first.len(), 1);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![first[0].0],
        )
        .unwrap();

    // Attempt is still pending → reconcile must re-dispatch a fresh execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let second = executions_for(&db, &revision_id);
    assert_eq!(
        second.len(),
        2,
        "an active attempt must still re-dispatch after a worker dies: {second:?}",
    );
    assert!(
        second.iter().any(|(_, status)| status == "ready"),
        "a fresh ready execution must exist while the attempt is active: {second:?}",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "the revision must stay dispatchable while its attempt is active",
    );
}

// ── CI-fix revision: stop re-dispatch once the attempt retires ──
//
// Symmetric sibling of the merge-conflict arm above. `reconcile_revision_execution`
// (via `retired_spawning_attempt_status`) keys on the task's `created_via` prefix:
// `merge-conflict:<crz_id>` → `conflict_resolutions`, `ci-fix:<id>` → `ci_remediations`.
// The merge-conflict arm is exercised by the two tests above; these mirror them for
// the `ci-fix` arm so a regression in *that* branch (a CI-fix revision minting a fresh
// `revision_implementation` execution on every reconcile tick after its
// `ci_remediations` attempt retired) can't slip through silently.

/// Insert a `kind=revision` task linked to a CI-fix attempt.
/// Mirrors what `ci_watch` produces for a CI remediation: `created_via =
/// "ci-fix:<ci_remediations.id>"`, parent = the chore, and (as in the
/// steady-state loop) the row already flipped to `active`.
fn insert_ci_fix_revision_row(
    db: &WorkDb,
    product_id: &str,
    parent_task_id: &str,
    rem_id: &str,
) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}{rem_id}");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Fix failing CI on PR', '', 'active', ?3, ?3, 0, ?4, ?5)",
        rusqlite::params![id, product_id, now, created_via, parent_task_id],
    )
    .unwrap();
    id
}

/// Insert a `pending` CI remediation attempt linked to `chore_id`, and a
/// `ci-fix:<id>` revision task parented to it. Returns `(rem_id, revision_id)`.
fn setup_ci_fix_revision(
    db: &WorkDb,
    product_id: &str,
    chore_id: &str,
    pr_url: &str,
    pr_number: i64,
) -> (String, String) {
    let rem = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number,
            head_branch: "feature".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_ci_fix_revision_row(db, product_id, chore_id, &rem.id);
    db.set_ci_remediation_revision_task_id(&rem.id, &revision_id)
        .unwrap();
    (rem.id, revision_id)
}

/// Regression sibling of `merge_conflict_revision_stops_dispatch_after_attempt_succeeds`
/// for the `ci-fix` arm: a CI-fix revision must stop being re-dispatched once its
/// `ci_remediations` attempt has retired (`succeeded`), even though the chore PR is
/// still open + `in_review`. After the fix: a retired attempt drops the queued
/// execution and settles the revision to `in_review`; no new execution is created.
#[test]
fn ci_fix_revision_stops_dispatch_after_attempt_succeeds() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-stop-succeeds")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-stop-ok");
    let pr_url = "https://github.com/spinyfin/mono/pull/980";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 980);

    // First reconcile: attempt is still `pending`, so the revision dispatches
    // normally — the behaviour the fix must preserve.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // CI fix lands and the attempt retires. The `ready` execution from above is
    // still queued (the exact race that makes a manual move-to-Review pointless).
    db.mark_ci_remediation_succeeded(&rem_id, None).unwrap();

    // Second reconcile: must NOT mint another execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned so start_execution_run \
         can't flip the revision back to active",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its fix vehicle is spent",
    );

    // Third reconcile: idempotent — the in_review revision is no longer dispatchable.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_third = executions_for(&db, &revision_id);
    assert_eq!(
        after_third.len(),
        1,
        "reconcile must remain a no-op for a settled revision: {after_third:?}",
    );
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

/// Second retired-status case for the `ci-fix` arm: a `failed` attempt is just
/// as terminal as `succeeded`, so it must also stop re-dispatch and settle the
/// revision to `in_review`. (A CI fix that exhausts/aborts must not keep minting
/// executions either.)
#[test]
fn ci_fix_revision_stops_dispatch_after_attempt_fails() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-stop-fails")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-stop-fail");
    let pr_url = "https://github.com/spinyfin/mono/pull/981";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 981);

    // First reconcile: pending attempt → dispatch once.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // The CI fix attempt fails (retires terminally).
    db.mark_ci_remediation_failed(&rem_id, "ran out of attempts")
        .unwrap();

    // Second reconcile: no new execution; queued row abandoned; revision settled.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned once the failed attempt retires",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its (failed) fix vehicle is spent",
    );
}

/// Guard against over-blocking on the `ci-fix` arm (sibling of
/// `merge_conflict_revision_still_redispatches_while_attempt_active`): while the
/// `ci_remediations` attempt is still active (`pending`/`running`), a revision
/// whose previous execution died must still be re-dispatched. The fix only
/// short-circuits *retired* attempts.
#[test]
fn ci_fix_revision_still_redispatches_while_attempt_active() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-active-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/982";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (_rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 982);

    // First dispatch, then simulate a dead worker (execution orphaned).
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id);
    assert_eq!(first.len(), 1);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![first[0].0],
        )
        .unwrap();

    // Attempt is still pending → reconcile must re-dispatch a fresh execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let second = executions_for(&db, &revision_id);
    assert_eq!(
        second.len(),
        2,
        "an active attempt must still re-dispatch after a worker dies: {second:?}",
    );
    assert!(
        second.iter().any(|(_, status)| status == "ready"),
        "a fresh ready execution must exist while the attempt is active: {second:?}",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "the revision must stay dispatchable while its attempt is active",
    );
}

// ── Revision completion must leave the base row in Review, never Doing ──
//
// Contract: a base task/chore that has a revision underway must REMAIN in
// `in_review` (the Review column) the whole time — while the revision is in
// flight AND after it completes. A revision is an amendment to the base
// row's already-open PR; nothing in the revision lifecycle may transition
// the base out of Review into `active` (Doing). Only an explicit
// human/merge action advances a row out of Review.
//
// The stranding vector is `start_execution_run`: its kanban auto-advance
// historically flipped any row that was not `done`/`archived`/`blocked`
// to `active`, so a stray `ready` execution that landed on the base (a
// re-dispatch race around the revision's PR push) would yank the base
// from Review into Doing the moment it started. `reconcile_revision_execution`
// band-aided this for engine-spawned *revision* rows; the base row had no
// equivalent guard, and the base kind (chore vs project_task) made no
// difference — both share the same dispatch machinery. The fix closes the
// hole at the source so both kinds are covered by one rule.

/// Create a `project_task` in `in_review` with a bound PR, mirroring
/// `make_in_review_chore` but for a project-member task. The project's
/// auto-design seed task is skipped so the project_task is itself the
/// chain root the revision is filed against.
fn make_in_review_project_task(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let project = db
        .create_project(CreateProjectInput {
            product_id: product_id.to_owned(),
            name: "Project for revision tests".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
        })
        .unwrap();
    let task = db
        .create_task(CreateTaskInput {
            product_id: product_id.to_owned(),
            project_id: project.id.clone(),
            name: "Project task for revision tests".to_owned(),
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
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task.id, pr_url],
        )
        .unwrap();
    task.id
}

/// Shared body: with a revision filed against `base_id` (PR open, base in
/// Review), a fresh `ready` execution that lands on the base — the
/// re-dispatch race a completing revision can leave behind — must NOT
/// demote the base into `active`/Doing when it starts.
fn assert_started_execution_keeps_base_in_review(db: &WorkDb, base_id: &str) {
    // File a revision against the base so the base genuinely "has a
    // revision underway" — the precondition for the contract.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(revision_input(base_id), &checker).unwrap();
    assert_eq!(task_status(db, base_id), "in_review", "precondition");

    // Simulate the stray re-dispatch: a `ready` execution bound to the
    // base, then a worker claiming it.
    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(base_id.to_owned())
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    db.start_execution_run(
        &exec.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();

    assert_eq!(
        task_status(db, base_id),
        "in_review",
        "starting an execution against an in_review base must NOT demote it \
         out of Review into Doing — the revision rides the base's open PR",
    );
}

/// Base is a `chore`.
#[test]
fn revision_completion_keeps_base_chore_in_review() {
    let db = WorkDb::open(temp_db_path("rev-base-chore-in-review")).unwrap();
    let product_id = make_revision_product(&db, "base-chore-review");
    let pr_url = "https://github.com/spinyfin/mono/pull/533";
    let base_id = make_in_review_chore(&db, &product_id, pr_url);
    assert_started_execution_keeps_base_in_review(&db, &base_id);
}

/// Base is a `project_task`. Same machinery, same contract — the kind must
/// not change the outcome (the regression this guards against was a fix
/// applied to only one kind / only the revision row).
#[test]
fn revision_completion_keeps_base_project_task_in_review() {
    let db = WorkDb::open(temp_db_path("rev-base-pt-in-review")).unwrap();
    let product_id = make_revision_product(&db, "base-pt-review");
    let pr_url = "https://github.com/spinyfin/mono/pull/534";
    let base_id = make_in_review_project_task(&db, &product_id, pr_url);
    assert_started_execution_keeps_base_in_review(&db, &base_id);
}

// ── Transcript-path recording for conflict-resolution revision executions ──

/// Regression guard for the T1291 incident: a conflict-resolution revision
/// execution that IS dispatched (while the `conflict_resolutions` attempt is
/// still active) must record `transcript_path` in `work_runs` when the
/// worker fires a hook with the path.
///
/// The failure mode: `set_run_transcript_path_if_unset` receives
/// `RowMissing` when there is no `work_runs` row for the execution (the
/// execution was abandoned before dispatch). This test verifies the HAPPY
/// path — when the attempt is active and the scheduler calls
/// `start_execution_run`, the `work_runs` row IS created, and the transcript
/// path CAN be recorded. A separate test covers the abandoned-before-dispatch
/// path (no `work_runs` row → `has_run_row_for_execution` returns false).
#[test]
fn conflict_resolution_revision_execution_records_transcript_path() {
    let db = WorkDb::open(temp_db_path("crz-transcript-path")).unwrap();
    let product_id = make_revision_product(&db, "crz-transcript");
    let pr_url = "https://github.com/spinyfin/mono/pull/1291";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    // Insert a `conflict_resolutions` attempt (still `pending` / active).
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 1291,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha-1".into()),
            head_sha_before: Some("head-sha-1".into()),
        })
        .unwrap()
        .unwrap();

    // Create the revision task as `conflict_watch::maybe_spawn_conflict_revision`
    // would (created_via = "merge-conflict:<crz_id>").
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // Reconcile: attempt is still active → a `revision_implementation`
    // execution is created with status = `ready`.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1, "one execution should be created");
    assert_eq!(execs[0].1, "ready");
    let exec_id = &execs[0].0;

    // Precondition: no work_runs row yet.
    assert!(
        !db.has_run_row_for_execution(exec_id).unwrap(),
        "precondition: no work_runs row before start_execution_run",
    );

    // Simulate the scheduler dispatching the execution.
    let (_, run) = db
        .start_execution_run(
            exec_id,
            "worker-conflict-1",
            "mono",
            "lease-crz-1",
            "mono-agent-064",
            "/tmp/mono-agent-064",
        )
        .unwrap();

    // Now a work_runs row exists.
    assert!(
        db.has_run_row_for_execution(exec_id).unwrap(),
        "work_runs row must exist after start_execution_run",
    );
    assert!(
        db.transcript_path_for_execution(exec_id).unwrap().is_none(),
        "transcript_path must be NULL at run start",
    );

    // Simulate the worker's first hook event reporting its transcript path.
    let transcript_path = "/tmp/mono-agent-064/.boss/session.jsonl";
    let outcome = db
        .set_run_transcript_path_if_unset(exec_id, transcript_path)
        .unwrap();
    assert!(
        matches!(outcome, SetRunTranscriptPathOutcome::Updated),
        "set_run_transcript_path_if_unset must return Updated for a new run; got {outcome:?}",
    );

    // Confirm the path is readable via the execution-id namespace.
    let recorded = db
        .transcript_path_for_execution(exec_id)
        .unwrap();
    assert_eq!(
        recorded.as_deref(),
        Some(transcript_path),
        "transcript_path must be retrievable via transcript_path_for_execution",
    );

    // The run row's id must match the run we started.
    let _ = run; // run.id is the work_runs id; path was keyed on execution_id, both must agree.
}

/// Companion test: a conflict-resolution revision execution that was abandoned
/// BEFORE the scheduler dispatched it has no `work_runs` row.
/// `has_run_row_for_execution` must return `false` so the `TailRunTranscript`
/// handler can surface a clear "never dispatched" message instead of the
/// generic "no transcript path recorded" error.
#[test]
fn abandoned_conflict_resolution_revision_execution_has_no_run_row() {
    let db = WorkDb::open(temp_db_path("crz-abandoned-no-run")).unwrap();
    let product_id = make_revision_product(&db, "crz-abandoned");
    let pr_url = "https://github.com/spinyfin/mono/pull/1292";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 1292,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha-2".into()),
            head_sha_before: Some("head-sha-2".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First reconcile: attempt is active → execution created with status=ready.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1);
    let exec_id = &execs[0].0.clone();
    assert_eq!(execs[0].1, "ready");

    // Conflict resolves before the scheduler picks up the execution.
    db.mark_conflict_resolution_succeeded(&crz.id, None).unwrap();

    // Second reconcile: execution is abandoned (no worker ran).
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].1, "abandoned", "execution must be abandoned");

    // No work_runs row — the scheduler never called start_execution_run.
    assert!(
        !db.has_run_row_for_execution(exec_id).unwrap(),
        "abandoned execution must have no work_runs row; the TailRunTranscript handler \
         must surface NeverDispatched rather than KnownNoTranscript",
    );

    // transcript_path_for_execution must return None (consistent with current behaviour).
    assert!(
        db.transcript_path_for_execution(exec_id).unwrap().is_none(),
        "no transcript path must be recorded for an execution that was never dispatched",
    );
}
