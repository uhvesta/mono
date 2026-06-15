use super::*;

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
    db.set_metadata("live_status_disabled_slots", "1,3,7").unwrap();
    let value = db.get_metadata("live_status_disabled_slots").unwrap();
    assert_eq!(value.as_deref(), Some("1,3,7"));
}

#[test]
fn metadata_set_replaces_prior_value_for_same_key() {
    let path = temp_db_path("meta-replace");
    let db = WorkDb::open(path).unwrap();
    db.set_metadata("live_status_disabled_slots", "1,3").unwrap();
    db.set_metadata("live_status_disabled_slots", "5,7").unwrap();
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Backend schema")
                .build(),
        )
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Cleanup")
                .build(),
        )
        .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    assert_eq!(tree.projects.len(), 1);
    // Each project carries an auto-created `kind = 'design'` task
    // at `ordinal = 0` plus the user-created task — so the tree
    // sees both. The design task always sorts first.
    assert_eq!(tree.tasks.len(), 2);
    assert_eq!(tree.tasks[0].kind, TaskKind::Design);
    assert_eq!(tree.tasks[1].id, task.id);
    assert_eq!(tree.chores.len(), 1);
    assert_eq!(tree.chores[0].id, chore.id);

    db.delete_work_item(&chore.id).unwrap();
    let tree = db.get_work_tree(&product.id).unwrap();
    assert!(tree.chores.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn restore_work_item_clears_tombstone() {
    let path = temp_db_path("restore");
    let db = WorkDb::open(path.clone()).unwrap();

    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Recover me")
                .build(),
        )
        .unwrap();
    let short_id = chore.short_id.expect("chore has a short id");

    db.delete_work_item(&chore.id).unwrap();
    // Live listing hides it; the include-deleted listing surfaces it
    // with the tombstone populated.
    assert!(db.list_chores(&product.id, None, false).unwrap().is_empty());
    let deleted_view = db.list_chores(&product.id, None, true).unwrap();
    assert_eq!(deleted_view.len(), 1);
    assert!(deleted_view[0].deleted_at.is_some());
    assert!(db.get_work_item(&chore.id).is_err());

    let item_id = |item: &WorkItem| match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.id.clone(),
        WorkItem::Product(p) => p.id.clone(),
        WorkItem::Project(p) => p.id.clone(),
    };

    // Restore by friendly short id — the friendly resolver must find
    // the tombstoned row.
    let restored = db.restore_work_item(&format!("T{short_id}")).unwrap();
    assert_eq!(item_id(&restored), chore.id);
    assert!(db.get_work_item(&chore.id).is_ok());
    assert_eq!(db.list_chores(&product.id, None, false).unwrap().len(), 1);

    // Idempotent: restoring an already-live row succeeds as a no-op,
    // and a canonical id works just as well as the friendly form.
    let again = db.restore_work_item(&chore.id).unwrap();
    assert_eq!(item_id(&again), chore.id);

    // An id that matches no row is an error, not a silent no-op.
    assert!(db.restore_work_item("task_doesnotexist").is_err());

    let _ = std::fs::remove_file(path);
}

#[test]
fn delete_parent_cascades_to_revisions_and_restore_brings_them_back() {
    let db = WorkDb::open(temp_db_path("cascade-delete-revisions")).unwrap();
    let product_id = make_revision_product(&db, "cascade-rev");
    let pr_url = "https://github.com/spinyfin/mono/pull/9001";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    let r2 = db.create_revision(revision_input(&r1.id), &checker).unwrap();

    // Sanity: all three are live before the delete.
    assert!(db.get_work_item(&parent_id).is_ok());
    assert!(db.get_work_item(&r1.id).is_ok());
    assert!(db.get_work_item(&r2.id).is_ok());

    db.delete_work_item(&parent_id).unwrap();

    // Parent and both revisions must now be tombstoned.
    assert!(db.get_work_item(&parent_id).is_err());
    assert!(db.get_work_item(&r1.id).is_err());
    assert!(db.get_work_item(&r2.id).is_err());

    let deleted = db.list_chores(&product_id, None, true).unwrap();
    assert_eq!(deleted.len(), 1, "parent visible in include-deleted list");
    assert!(deleted[0].deleted_at.is_some());

    // The revisions also have deleted_at set.
    let conn = db.connect().unwrap();
    let r1_deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&r1.id], |r| r.get(0))
        .unwrap();
    let r2_deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&r2.id], |r| r.get(0))
        .unwrap();
    assert!(r1_deleted_at.is_some(), "r1 must be tombstoned");
    assert!(r2_deleted_at.is_some(), "r2 must be tombstoned");
    // All three rows share the same deleted_at timestamp (set in one transaction).
    assert_eq!(deleted[0].deleted_at, r1_deleted_at);
    assert_eq!(deleted[0].deleted_at, r2_deleted_at);
    drop(conn);

    // Restoring the parent brings both revisions back.
    db.restore_work_item(&parent_id).unwrap();

    assert!(db.get_work_item(&parent_id).is_ok());
    assert!(db.get_work_item(&r1.id).is_ok());
    assert!(db.get_work_item(&r2.id).is_ok());

    let conn = db.connect().unwrap();
    let r1_deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&r1.id], |r| r.get(0))
        .unwrap();
    let r2_deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&r2.id], |r| r.get(0))
        .unwrap();
    assert!(r1_deleted_at.is_none(), "r1 deleted_at must be cleared");
    assert!(r2_deleted_at.is_none(), "r2 deleted_at must be cleared");
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .map(|i| {
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(format!("Task {i}"))
                .description(format!("d{i}"))
                .autostart(i % 2 == 0)
                .build()
        })
        .collect::<Vec<_>>();
    let created = db.create_many_tasks(CreateManyTasksInput { items: inputs }).unwrap();

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

    let tasks = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    // Five user-created tasks plus the auto-created design task
    // that every new project carries.
    assert_eq!(tasks.len(), 6);
    assert!(tasks.iter().any(|t| t.kind == TaskKind::Design));

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(project.id.clone())
            .name("Good")
            .build(),
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id("proj_does_not_exist")
            .name("Bad")
            .build(),
    ];
    let err = db
        .create_many_tasks(CreateManyTasksInput { items: inputs })
        .expect_err("expected rollback");
    let msg = format!("{err:#}");
    assert!(msg.contains("item 1"), "error must name failing index: {msg}");

    let tasks = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    // The batch's project_task inserts must roll back, but the
    // auto-created design task (inserted in `create_project`'s
    // own committed transaction) is not part of this batch and
    // remains. Assert exactly that shape so a future regression
    // that lets the Bad row leak out shows up.
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].kind, TaskKind::Design);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();

    let inputs = (0..3)
        .map(|i| {
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("Chore {i}"))
                .autostart(false)
                .build()
        })
        .collect::<Vec<_>>();
    let created = db.create_many_chores(CreateManyChoresInput { items: inputs }).unwrap();
    assert_eq!(created.len(), 3);
    for chore in &created {
        assert_eq!(chore.kind, TaskKind::Chore);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore_idle = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Idle")
                .build(),
        )
        .unwrap();
    let chore_running = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Running")
                .build(),
        )
        .unwrap();
    db.reconcile_product_executions(&product.id).unwrap();

    // Drive the second chore's execution into a running run.
    let running_execution = db.list_executions(Some(&chore_running.id)).unwrap().pop().unwrap();
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
    assert_eq!(runtime_idle.execution_status, Some(ExecutionStatus::Ready));
    assert_eq!(runtime_idle.run_status, None);

    let runtime_running = tree
        .task_runtimes
        .iter()
        .find(|r| r.work_item_id == chore_running.id)
        .expect("missing running runtime entry");
    assert_eq!(runtime_running.execution_status, Some(ExecutionStatus::Running));
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Investigate")
                .build(),
        )
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
    assert_eq!(after_ready.execution_status, Some(ExecutionStatus::Ready));
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
    assert_eq!(after_run.execution_status, Some(ExecutionStatus::Running));
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let prereq = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Prereq")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let dependent = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Dependent")
                .autostart(false)
                .build(),
        )
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let other_prereq = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(other_product.id.clone())
                .name("Other Prereq")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let other_dependent = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(other_product.id.clone())
                .name("Other Dependent")
                .autostart(false)
                .build(),
        )
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("One")
                .build(),
        )
        .unwrap();
    let second = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Two")
                .build(),
        )
        .unwrap();

    db.reorder_project_tasks(&project.id, &[second.id.clone(), first.id.clone()])
        .unwrap();

    // The design task always sits at `ordinal = 0`, so it stays
    // at index 0 regardless of how the user-created project_tasks
    // are reordered. The reorder swap applies to the project_task
    // pair only, which now occupy indices 1 and 2.
    let tree = db.get_work_tree(&product.id).unwrap();
    assert_eq!(tree.tasks[0].kind, TaskKind::Design);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Schema")
                .build(),
        )
        .unwrap();

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Ready)
                .cube_repo_id("cube_repo_mono")
                .workspace_path("/tmp/mono-agent-001")
                .build(),
        )
        .unwrap();
    assert_eq!(execution.repo_remote_url, "git@github.com:spinyfin/mono.git");

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
    assert_eq!(db.get_attention_item(&attention.id).unwrap().title, "Need product call");

    let _ = std::fs::remove_file(path);
}

#[test]
fn product_worker_branch_prefix_canonicalises_trailing_slash() {
    let path = temp_db_path("prefix-canonicalise");
    let db = WorkDb::open(path.clone()).unwrap();
    // Caller omits the trailing slash — it must be added on write.
    let (product, _) = product_task_execution_with_prefix(&db, Some("bduff"));
    assert_eq!(product.worker_branch_prefix.as_deref(), Some("bduff/"));
    // Re-reading the product yields the canonical value too.
    let reloaded = db.get_product(&product.id).unwrap().unwrap();
    assert_eq!(reloaded.worker_branch_prefix.as_deref(), Some("bduff/"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn execution_honors_product_worker_branch_prefix() {
    // The product's `worker_branch_prefix` is frozen onto the execution
    // row at creation and drives branch naming under the default
    // `BossExecPrefix` strategy: the worker pushes to
    // `<prefix>exec_<id>` (e.g. `bduff/exec_<id>`), not `boss/exec_<id>`.
    // Regression test for #1141, where the configured prefix was being
    // ignored at branch-creation time.
    let path = temp_db_path("prefix-denormalise");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_, execution) = product_task_execution_with_prefix(&db, Some("bduff/"));
    // The prefix is denormalised onto the execution row.
    assert_eq!(execution.worker_branch_prefix.as_deref(), Some("bduff/"));
    // No editorial_rules configured → branch_naming defaults to
    // BossExecPrefix, under which the frozen prefix replaces `boss/`.
    assert_eq!(execution.branch_naming, BranchNaming::BossExecPrefix);
    let branch = crate::completion::expected_branch_name(
        &execution.id,
        &execution.branch_naming,
        execution.worker_branch_prefix.as_deref(),
    );
    assert_eq!(branch, format!("bduff/{}", execution.id));
    let _ = std::fs::remove_file(path);
}

#[test]
fn execution_without_product_prefix_defaults_to_boss() {
    let path = temp_db_path("prefix-default");
    let db = WorkDb::open(path.clone()).unwrap();
    let (product, execution) = product_task_execution_with_prefix(&db, None);
    assert_eq!(product.worker_branch_prefix, None);
    // No override frozen onto the execution → default BossExecPrefix shape.
    assert_eq!(execution.worker_branch_prefix, None);
    assert_eq!(execution.branch_naming, BranchNaming::BossExecPrefix);
    let branch = crate::completion::expected_branch_name(
        &execution.id,
        &execution.branch_naming,
        execution.worker_branch_prefix.as_deref(),
    );
    assert_eq!(branch, format!("boss/{}", execution.id));
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();

    let err = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(product.id.clone())
                .kind(ExecutionKind::ProjectDesign)
                .build(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("does not resolve to a repo_remote_url"),
        "expected resolver error in `{err}`",
    );

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(product.id.clone())
                .kind(ExecutionKind::ProjectDesign)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    assert_eq!(execution.repo_remote_url, "git@github.com:spinyfin/mono.git");

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("First")
                .build(),
        )
        .unwrap();
    let second_task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Second")
                .build(),
        )
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Cleanup")
                .build(),
        )
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
    assert_eq!(first_execution[0].kind, ExecutionKind::TaskImplementation);
    assert_eq!(first_execution[0].status, ExecutionStatus::Ready);

    let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
    assert_eq!(second_execution.len(), 1);
    assert_eq!(second_execution[0].status, ExecutionStatus::WaitingDependency);

    let chore_execution = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(chore_execution.len(), 1);
    assert_eq!(chore_execution[0].kind, ExecutionKind::ChoreImplementation);
    assert_eq!(chore_execution[0].status, ExecutionStatus::Ready);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("First")
                .build(),
        )
        .unwrap();
    let second_task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Second")
                .build(),
        )
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
    assert_eq!(result.updated[0].status, ExecutionStatus::Ready);

    let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
    assert_eq!(second_execution[0].status, ExecutionStatus::Ready);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            driver: None,
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
    assert_eq!(task_execution[0].status, ExecutionStatus::Ready);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Nimbus migration")
                .repo_remote_url("git@github.com:myorg/nimbus.git")
                .build(),
        )
        .unwrap();

    let result = db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(result.created.len(), 1, "chore should dispatch on first pass");
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(
        executions[0].repo_remote_url, "git@github.com:myorg/nimbus.git",
        "the row should carry the chore's override, not the product default",
    );
    assert_eq!(executions[0].status, ExecutionStatus::Ready);

    // No sticky attention items raised for a resolvable row.
    assert!(db.list_attention_items_for_work_item(&chore.id).unwrap().is_empty(),);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert_eq!(db.list_attention_items_for_work_item(&chore_id).unwrap().len(), 1,);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            RequestExecutionInput::builder().work_item_id(chore_id.clone()).build(),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert_eq!(db.list_attention_items_for_work_item(&chore_id).unwrap().len(), 1,);

    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            repo_remote_url: Some("git@github.com:myorg/nimbus.git".to_owned()),
            effort_level: None,
            model_override: None,
            driver: None,
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let second_pass = db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(second_pass.created.len(), 1);
    let executions = db.list_executions(Some(&chore_id)).unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].repo_remote_url, "git@github.com:myorg/nimbus.git",);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Cleanup")
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
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
    assert_eq!(execution.status, ExecutionStatus::Running);
    assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    assert_eq!(execution.workspace_path.as_deref(), Some("/tmp/mono-agent-001"));
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
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
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
        .set_run_transcript_path_if_unset(&execution.id, "/home/u/.claude/projects/foo/sess-1.jsonl")
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
        .set_run_transcript_path_if_unset(&execution.id, "/home/u/.claude/projects/foo/sess-2.jsonl")
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
        db.transcript_path_for_execution(&execution.id).unwrap().as_deref(),
        Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        "read-side lookup must accept an execution id and return the latest run's transcript_path",
    );
    assert!(
        db.transcript_path_for_execution(&run.id).unwrap().is_none(),
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
    let wrong_namespace = db.set_run_transcript_path_if_unset(&run.id, "/y.jsonl").unwrap();
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Cleanup")
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
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
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore/task, got {other:?}"),
    }

    let cancelled = db.cancel_execution(&execution.id).unwrap();
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    assert!(cancelled.finished_at.is_some());

    // Active → todo so the kanban card returns to the To-Do lane.
    match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Todo),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Has PR")
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
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
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore/task, got {other:?}"),
    }

    let _ = std::fs::remove_file(path);
}

/// The "AI reviewing" badge (`ai_reviewing`) must be honest: it may only show
/// when a reviewer agent is actually in flight (`pr_review` execution
/// `running`), never while the review is merely enqueued or stuck in the
/// pre-start retry loop (`ready`). Regression for the jj-immutable-head
/// dispatch bug, where a `pr_review` exec bounced ready→fail→ready and the card
/// lied "AI reviewing" the whole time even though nothing was reviewing.
#[test]
fn ai_reviewing_badge_only_shows_while_reviewer_running() {
    let path = temp_db_path("ai-reviewing-running-only");
    let db = WorkDb::open(path.clone()).unwrap();

    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Has PR under review")
                .autostart(false)
                .build(),
        )
        .unwrap();

    // Simulate the P992 `PendingReview` hold: the implementation finished and
    // opened a PR, but the card is held in Doing (`active`) with the PR stamped
    // while the reviewer pass runs. This is the only state the badge keys on.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, "https://github.com/spinyfin/mono/pull/4242"],
        )
        .unwrap();
    }

    // Reviewer enqueued but not yet dispatched (`ready`). Nothing is reviewing,
    // so the badge must stay off.
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree
        .chores
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore present in work tree");
    assert!(
        !card.ai_reviewing,
        "a `ready` (queued / retrying) reviewer must NOT show the AI-reviewing badge"
    );

    // Reviewer agent actually starts → exec `running`. Now the badge is honest.
    let (_, run) = db
        .start_execution_run(
            &review.id,
            "worker-rev",
            "mono",
            "lease-rev",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree
        .chores
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore present in work tree");
    assert!(
        card.ai_reviewing,
        "a `running` reviewer must show the AI-reviewing badge"
    );

    // Reviewer finishes (terminal) → badge off again.
    db.finish_execution_run(
        &review.id,
        &run.id,
        ExecutionStatus::Completed,
        "completed",
        None,
        None,
        true,
        None,
    )
    .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree
        .chores
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore present in work tree");
    assert!(
        !card.ai_reviewing,
        "a finished (terminal) reviewer must NOT show the AI-reviewing badge"
    );

    let _ = std::fs::remove_file(path);
}

/// Regression for T1647: the "AI reviewing" badge (`ai_reviewing`) must
/// remain visible after `finish_execution_run(Running)` — the post-spawn
/// state `PaneSpawnRunner` now writes for `pr_review` executions via
/// `RunWaitState::ReviewerPaneAlive`. Before the fix, `PaneSpawnRunner`
/// returned `WaitingHuman`, immediately flipping the execution to
/// `waiting_human` after spawn; the badge queries only `running`, so it
/// disappeared the moment the pane was spawned even though the reviewer
/// was still actively working.
#[test]
fn ai_reviewing_badge_shows_after_reviewer_pane_spawn() {
    let path = temp_db_path("ai-reviewing-post-spawn");
    let db = WorkDb::open(path.clone()).unwrap();

    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Review in progress")
                .autostart(false)
                .build(),
        )
        .unwrap();

    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, "https://github.com/spinyfin/mono/pull/1647"],
        )
        .unwrap();
    }

    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    // Dispatch: `start_execution_run` → `running`.
    let (_, run) = db
        .start_execution_run(
            &review.id,
            "review-17",
            "mono",
            "lease-review-17",
            "mono-agent-017",
            "/tmp/mono-agent-017",
        )
        .unwrap();

    // Simulate `PaneSpawnRunner` returning `ReviewerPaneAlive`: the run is
    // recorded as completed but the execution stays `running`.
    db.finish_execution_run(
        &review.id,
        &run.id,
        ExecutionStatus::Running,
        "completed",
        Some("Spawned reviewer pane in slot 17."),
        None,
        false,
        None,
    )
    .unwrap();

    // Badge must be visible: reviewer pane is alive and working.
    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree
        .chores
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore present in work tree");
    assert!(
        card.ai_reviewing,
        "badge must show after post-spawn finish_execution_run(Running); \
         was the execution accidentally moved to waiting_human instead of staying running?"
    );

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Already done")
                .build(),
        )
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
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
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
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Done),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Stranded chore")
                .build(),
        )
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
    assert_eq!(executions[0].status, ExecutionStatus::Ready);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Bounced chore")
                .build(),
        )
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
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Failed)
            .build(),
    )
    .unwrap();

    let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
    assert_eq!(redispatched, vec![chore.id.clone()]);

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 2, "expected the failed exec plus a fresh ready one");
    let latest = executions.last().unwrap();
    assert_eq!(latest.status, ExecutionStatus::Ready);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Live chore")
                .build(),
        )
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
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::WaitingHuman)
            .build(),
    )
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Stale chore")
                .build(),
        )
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
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
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
    assert_eq!(stale_after.status, ExecutionStatus::Abandoned);
    let latest = executions.last().unwrap();
    assert_ne!(latest.id, stale.id);
    assert_eq!(latest.status, ExecutionStatus::Ready);
}

/// Helper: product + chore + a started run on `host_id`, returning the
/// execution id. The run lands in `work_runs` with status `active`.
fn start_run_on_host_for_test(db: &WorkDb, host_id: &str) -> String {
    let product = db
        .create_product(CreateProductInput {
            name: "p".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("c")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    db.start_execution_run_on_host(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "ws-1",
        "/tmp/ws-1",
        host_id,
    )
    .unwrap();
    execution.id
}

#[test]
fn latest_run_host_for_execution_reports_dispatch_host() {
    let db = WorkDb::open(temp_db_path("latest-run-host")).unwrap();
    let exec_id = start_run_on_host_for_test(&db, "zakalwe");
    assert_eq!(
        db.latest_run_host_for_execution(&exec_id).unwrap().as_deref(),
        Some("zakalwe"),
    );
    // An execution with no run yet resolves to None.
    assert_eq!(db.latest_run_host_for_execution("exec_does_not_exist").unwrap(), None,);
}

#[test]
fn set_run_remote_pid_updates_latest_run() {
    let db = WorkDb::open(temp_db_path("set-remote-pid")).unwrap();
    let exec_id = start_run_on_host_for_test(&db, "zakalwe");
    assert!(
        db.set_run_remote_pid_for_execution(&exec_id, 4242).unwrap(),
        "stamping a pid onto an existing run must report it updated a row",
    );
    // No run for this execution → false (benign no-op).
    assert!(!db.set_run_remote_pid_for_execution("exec_does_not_exist", 7).unwrap(),);
}

#[test]
fn list_reattachable_remote_runs_filters_local_and_terminal() {
    let db = WorkDb::open(temp_db_path("reattachable")).unwrap();
    let remote_exec = start_run_on_host_for_test(&db, "zakalwe");
    let local_exec = start_run_on_host_for_test(&db, "local");

    let runs = db.list_reattachable_remote_runs().unwrap();
    let exec_ids: Vec<&str> = runs.iter().map(|r| r.execution_id.as_str()).collect();
    assert!(
        exec_ids.contains(&remote_exec.as_str()),
        "an active remote run must be reattachable, got {exec_ids:?}",
    );
    assert!(
        !exec_ids.contains(&local_exec.as_str()),
        "a local run must never be reattachable",
    );
    let remote_handle = runs.iter().find(|r| r.execution_id == remote_exec).unwrap();
    assert_eq!(remote_handle.host_id, "zakalwe");

    // Settling the execution removes it from the reattachable set.
    db.mark_execution_orphaned(&remote_exec, "test: settled").unwrap();
    let after = db.list_reattachable_remote_runs().unwrap();
    assert!(
        !after.iter().any(|r| r.execution_id == remote_exec),
        "a run whose execution has settled must not be reattachable",
    );
}
