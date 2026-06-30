//! Tests for `completed_at` semantics:
//! - set once on terminal transition (done/archived/cancelled), never re-bumped (COALESCE)
//! - cleared to NULL on re-open
//! - engine PR-merge path (mark_chore_pr_merged → done) sets it
//! - engine flip_in_review_revisions_to_done sets it
//! - reconciler_close_work_item sets it
//! - migrate_tasks_completed_at backfills terminal rows from created_at, leaves others NULL

use super::*;

fn task_completed_at(db: &WorkDb, task_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT completed_at FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// mark_chore_pr_merged (PR-merge → done path) must set completed_at on the
/// parent chore. This is the exact engine path that the bug's 79 followup
/// chores traveled — it was the primary missing site.
#[test]
fn mark_chore_pr_merged_sets_completed_at_on_parent() {
    let db = WorkDb::open(temp_db_path("mcp-completed-at-parent")).unwrap();
    let product_id = make_revision_product(&db, "mcp-cat-p");
    let pr_url = "https://github.com/spinyfin/mono/pull/9001";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    assert!(
        task_completed_at(&db, &parent_id).is_none(),
        "completed_at must be NULL before merge",
    );

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let completed = task_completed_at(&db, &parent_id);
    assert!(
        completed.is_some(),
        "completed_at must be set after mark_chore_pr_merged",
    );
}

/// A second unrelated UPDATE on a done row must NOT overwrite completed_at
/// (the COALESCE guard). Simulate this by calling mark_chore_pr_merged twice
/// (the second call is a no-op on status but tests that completed_at is stable).
#[test]
fn completed_at_not_re_bumped_by_coalesce() {
    let db = WorkDb::open(temp_db_path("mcp-completed-at-coalesce")).unwrap();
    let product_id = make_revision_product(&db, "mcp-cat-c");
    let pr_url = "https://github.com/spinyfin/mono/pull/9002";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();
    let first = task_completed_at(&db, &parent_id);
    assert!(first.is_some(), "completed_at must be set after first merge");

    // Manually bump updated_at to simulate a bulk mutation touching done rows.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET updated_at = '9999999999' WHERE id = ?1",
        rusqlite::params![parent_id],
    )
    .unwrap();
    drop(conn);

    // completed_at must be unchanged.
    let after_bump = task_completed_at(&db, &parent_id);
    assert_eq!(
        first, after_bump,
        "completed_at must not change when updated_at is re-stamped on a done row",
    );
}

/// flip_in_review_revisions_to_done (called inside mark_chore_pr_merged) must
/// also set completed_at on the in_review revision that rides the parent PR.
#[test]
fn flip_in_review_revision_to_done_sets_completed_at() {
    let db = WorkDb::open(temp_db_path("flip-rev-completed-at")).unwrap();
    let product_id = make_revision_product(&db, "flip-cat");
    let pr_url = "https://github.com/spinyfin/mono/pull/9003";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Put the revision in_review so flip_in_review_revisions_to_done picks it up.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    assert!(
        task_completed_at(&db, &revision.id).is_none(),
        "completed_at must be NULL before parent merges",
    );

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // Revision must be done with completed_at set.
    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(rev_after.status, TaskStatus::Done);
    assert!(
        task_completed_at(&db, &revision.id).is_some(),
        "completed_at must be set when flip_in_review_revisions_to_done fires",
    );
}

/// reconciler_close_work_item (external-tracker close / PR-merge-close path)
/// must set completed_at when it transitions a row to done.
#[test]
fn reconciler_close_work_item_sets_completed_at() {
    let db = WorkDb::open(temp_db_path("reconciler-close-completed-at")).unwrap();
    let product_id = make_revision_product(&db, "rec-close-cat");
    let pr_url = "https://github.com/spinyfin/mono/pull/9004";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    assert!(
        task_completed_at(&db, &chore_id).is_none(),
        "completed_at must be NULL before close",
    );

    let closed = db.reconciler_close_work_item(&chore_id).unwrap();
    assert!(
        closed,
        "reconciler_close_work_item must return true for an in_review row"
    );

    assert!(
        task_completed_at(&db, &chore_id).is_some(),
        "completed_at must be set by reconciler_close_work_item",
    );
}

/// re-opening a done row must clear completed_at back to NULL.
#[test]
fn reopen_done_row_clears_completed_at() {
    let db = WorkDb::open(temp_db_path("reopen-clears-completed-at")).unwrap();
    let product_id = make_revision_product(&db, "reopen-cat");
    let pr_url = "https://github.com/spinyfin/mono/pull/9005";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    db.mark_chore_pr_merged(&chore_id, pr_url).unwrap();
    assert!(
        task_completed_at(&db, &chore_id).is_some(),
        "completed_at must be set after merge",
    );

    // Move the row back to todo via the manual update path (simulates operator re-open).
    db.update_task(
        &chore_id,
        WorkItemPatch {
            status: Some("todo".to_owned()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();

    assert!(
        task_completed_at(&db, &chore_id).is_none(),
        "completed_at must be cleared (NULL) after re-open to todo",
    );
}

/// migrate_tasks_completed_at must backfill existing terminal rows from
/// created_at (NOT updated_at — that would reproduce the original bug) and
/// leave non-terminal rows NULL.
#[test]
fn migration_completed_at_backfills_from_created_at_not_updated_at() {
    let path = disk_db_path("migrate-completed-at-backfill");
    let conn = rusqlite::Connection::open(&path).unwrap();

    // Build a minimal schema without completed_at (simulates pre-v21 DB).
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
         -- done row: created_at=100, updated_at=999 (the buggy re-stamp).
         INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at)
             VALUES ('t_done', 'prod_1', 'chore', 'done-task', 'done', '100', '999');
         -- archived row
         INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at)
             VALUES ('t_arch', 'prod_1', 'chore', 'arch-task', 'archived', '200', '888');
         -- active (non-terminal) row — must remain NULL
         INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at)
             VALUES ('t_active', 'prod_1', 'chore', 'active-task', 'active', '300', '300');
         INSERT INTO metadata(key, value) VALUES ('schema_version', '20');",
    )
    .unwrap();
    drop(conn);

    // Open via WorkDb — this runs all migrations including migrate_tasks_completed_at.
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();

    // done row: completed_at must equal created_at (100), NOT updated_at (999).
    let done_completed: Option<String> = conn
        .query_row("SELECT completed_at FROM tasks WHERE id = 't_done'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        done_completed.as_deref(),
        Some("100"),
        "backfill must use created_at (100) for done rows, not updated_at (999)",
    );

    // archived row: completed_at must equal created_at (200).
    let arch_completed: Option<String> = conn
        .query_row("SELECT completed_at FROM tasks WHERE id = 't_arch'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        arch_completed.as_deref(),
        Some("200"),
        "backfill must use created_at (200) for archived rows",
    );

    // active row: completed_at must be NULL.
    let active_completed: Option<String> = conn
        .query_row("SELECT completed_at FROM tasks WHERE id = 't_active'", [], |r| r.get(0))
        .unwrap();
    assert!(
        active_completed.is_none(),
        "completed_at must remain NULL for non-terminal rows after migration",
    );

    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// record_worker_pr_completion with WorkerPrCompletionTarget::Done must set
/// completed_at on the task. This is the primary path for tasks whose PR was
/// already merged at Stop time (PrStatus::Merged → Done in completion.rs).
#[test]
fn record_worker_pr_completion_done_sets_completed_at() {
    let db = WorkDb::open(temp_db_path("rwpc-done-completed-at")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwpc-done");
    let pr_url = "https://github.com/spinyfin/mono/pull/9010";

    assert!(
        task_completed_at(&db, &chore_id).is_none(),
        "completed_at must be NULL before completion",
    );

    db.record_worker_pr_completion(&exec_id, pr_url, None, WorkerPrCompletionTarget::Done)
        .unwrap();

    assert!(
        task_completed_at(&db, &chore_id).is_some(),
        "completed_at must be set after record_worker_pr_completion with target=Done",
    );
}

/// A second record_worker_pr_completion call on an already-done task must NOT
/// re-bump completed_at (COALESCE stability).
#[test]
fn record_worker_pr_completion_done_coalesce_stability() {
    let db = WorkDb::open(temp_db_path("rwpc-done-coalesce")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwpc-coalesce");
    let pr_url = "https://github.com/spinyfin/mono/pull/9011";

    db.record_worker_pr_completion(&exec_id, pr_url, None, WorkerPrCompletionTarget::Done)
        .unwrap();
    let first = task_completed_at(&db, &chore_id);
    assert!(first.is_some(), "completed_at must be set after first completion");

    // Simulate a bulk re-stamp of updated_at (the original bug trigger).
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET updated_at = '9999999999' WHERE id = ?1",
        rusqlite::params![chore_id],
    )
    .unwrap();
    drop(conn);

    let after_bump = task_completed_at(&db, &chore_id);
    assert_eq!(
        first, after_bump,
        "completed_at must not change when updated_at is re-stamped on a done row",
    );
}

/// record_worker_no_op_completion must set completed_at when it closes a
/// non-terminal task as done. This is the path for workers that detect the
/// change is already present on main (completion.rs:3685).
#[test]
fn record_worker_no_op_completion_sets_completed_at() {
    let db = WorkDb::open(temp_db_path("rwnoc-completed-at")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwnoc");

    assert!(
        task_completed_at(&db, &chore_id).is_none(),
        "completed_at must be NULL before no-op completion",
    );

    db.record_worker_no_op_completion(&exec_id, "already done on main")
        .unwrap();

    assert!(
        task_completed_at(&db, &chore_id).is_some(),
        "completed_at must be set after record_worker_no_op_completion",
    );
}

/// A second record_worker_no_op_completion on an already-terminal task must
/// return Ok(None) (idempotent) and leave completed_at unchanged.
#[test]
fn record_worker_no_op_completion_coalesce_stability() {
    let db = WorkDb::open(temp_db_path("rwnoc-coalesce")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwnoc-c");

    db.record_worker_no_op_completion(&exec_id, "already done on main")
        .unwrap();
    let first = task_completed_at(&db, &chore_id);
    assert!(first.is_some(), "completed_at must be set after first no-op");

    // Simulate a bulk re-stamp of updated_at.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET updated_at = '9999999999' WHERE id = ?1",
        rusqlite::params![chore_id],
    )
    .unwrap();
    drop(conn);

    let after_bump = task_completed_at(&db, &chore_id);
    assert_eq!(
        first, after_bump,
        "completed_at must not change when updated_at is re-stamped on a done row",
    );
}

/// Idempotency: running migrate_tasks_completed_at a second time must not
/// overwrite already-set completed_at values (the COALESCE/column-exists guard).
#[test]
fn migration_completed_at_is_idempotent() {
    let path = disk_db_path("migrate-completed-at-idempotent");
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
         INSERT INTO tasks(id, product_id, kind, name, status, created_at, updated_at)
             VALUES ('t_done2', 'prod_1', 'chore', 'done2', 'done', '555', '999');
         INSERT INTO metadata(key, value) VALUES ('schema_version', '20');",
    )
    .unwrap();
    drop(conn);

    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    let first: Option<String> = conn
        .query_row("SELECT completed_at FROM tasks WHERE id = 't_done2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        first.as_deref(),
        Some("555"),
        "first migration must set completed_at = created_at"
    );
    drop(conn);

    // Re-open — migration guard skips because column already exists.
    let db2 = WorkDb::open(path.clone()).unwrap();
    let conn2 = db2.connect().unwrap();
    let second: Option<String> = conn2
        .query_row("SELECT completed_at FROM tasks WHERE id = 't_done2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        first, second,
        "re-running migration must not overwrite the existing completed_at value",
    );

    drop(conn2);
    let _ = std::fs::remove_file(&path);
}
