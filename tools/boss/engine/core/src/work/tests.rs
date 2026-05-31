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
    let tasks = db
        .list_tasks(&project.product_id, Some(project_id), None, false)
        .unwrap();
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

/// Helper: create a product (optionally with a worker branch prefix),
/// a project, a task, and an execution under it. Returns the stored
/// product and execution so prefix denormalisation can be asserted.
#[cfg(test)]
fn product_task_execution_with_prefix(
    db: &WorkDb,
    worker_branch_prefix: Option<&str>,
) -> (Product, WorkExecution) {
    let product = db
        .create_product(CreateProductInput {
            name: "Prefix Co".to_owned(),
            description: Some("desc".to_owned()),
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: worker_branch_prefix.map(str::to_owned),
        })
        .unwrap();
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".to_owned(),
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
            name: "T".to_owned(),
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
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
            prefer_is_soft: false,
            pr_url: None,
        
            allow_dirty: false,
        })
        .unwrap();
    (product, execution)
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        prefer_is_soft: false,
        pr_url: None,
    
        allow_dirty: false,
    })
    .unwrap()
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

// ── Bug A: double-spawn guard ───────────────────────────────────────────

fn make_waiting_human_chore(db: &WorkDb, label: &str) -> (String, String, String) {
    let product = db
        .create_product(CreateProductInput {
            name: format!("Prod-{label}"),
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
            name: format!("Chore-{label}"),
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
    let exec = db
        .create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".into(),
            status: Some("ready".into()),
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
            prefer_is_soft: false,
            pr_url: None,
        
            allow_dirty: false,
        })
        .unwrap();
    let (exec, run) = db
        .start_execution_run(
            &exec.id,
            "agent-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/workspaces/ws-1",
        )
        .unwrap();
    db.finish_execution_run(
        &exec.id,
        &run.id,
        "waiting_human",
        "completed",
        None,
        None,
        false,
        None,
    )
    .unwrap();
    (product.id, chore.id, exec.id)
}

// ── Bug B: late PR detection ────────────────────────────────────────────

fn make_abandoned_chore_with_workspace(db: &WorkDb, label: &str) -> (String, String, String) {
    let (product_id, chore_id, exec_id) = make_waiting_human_chore(db, label);
    // Simulate the orphan sweep abandoning exec_a.
    db.mark_execution_redundant(&exec_id).unwrap();
    (product_id, chore_id, exec_id)
}

// ── Revision tasks Phase 1: schema + chain_root ────────────────────────

/// Helper: create a minimal product for revision tests.
fn make_revision_product(db: &WorkDb, label: &str) -> String {
    db.create_product(CreateProductInput {
        name: format!("Boss-{label}"),
        description: None,
        repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
        design_repo: None,
        docs_repo: None,
        worker_branch_prefix: None,
    })
    .unwrap()
    .id
}

/// Helper: create a chore (non-revision root) and return its id.
fn make_chore_root(db: &WorkDb, product_id: &str, label: &str) -> String {
    db.create_chore(CreateChoreInput {
        product_id: product_id.to_owned(),
        name: format!("Root chore {label}"),
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
}

/// Helper: directly INSERT a revision task row (kind = 'revision') with
/// the given parent_task_id. Phase 2 will add `insert_revision_in_tx`;
/// for Phase 1 tests we bypass the API to keep the test self-contained.
fn insert_revision_row(db: &WorkDb, product_id: &str, parent_task_id: &str) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'Test revision', '', 'todo', ?3, ?3, ?4)",
            rusqlite::params![id, product_id, now, parent_task_id],
        )
        .unwrap();
    id
}

// ── Revision tasks Phase 2: CLI create-revision gate + insert ──────────

/// Helper: create a chore and set its pr_url (to simulate "in review").
fn make_in_review_chore(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product_id.to_owned(),
            name: "Chore for revision tests".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None, // inherits from product
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        rusqlite::params![chore.id, pr_url],
    )
    .unwrap();
    chore.id
}

/// Helper: create a chore whose status is `done` (simulates merged PR).
fn make_done_chore(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let id = make_in_review_chore(db, product_id, pr_url);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'done' WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
    id
}

/// Helper: build a minimal `CreateRevisionInput` for the given parent id.
fn revision_input(parent_id: &str) -> CreateRevisionInput {
    CreateRevisionInput {
        parent_task_id: parent_id.to_owned(),
        description: "test revision ask".to_owned(),
        name: None,
        priority: None,
        effort_level: None,
        model_override: None,
        force_duplicate: false,
        created_via: None,
    }
}

// ── attach_revision_projections ─────────────────────────────────────────

/// Build a minimal Task with enough fields for `attach_revision_projections`.
fn make_bare_task(id: &str, kind: &str, parent: Option<&str>, pr: Option<&str>, ts: &str) -> Task {
    Task {
        id: id.to_owned(),
        short_id: None,
        product_id: "p".to_owned(),
        project_id: None,
        kind: kind.to_owned(),
        name: "n".to_owned(),
        description: "d".to_owned(),
        status: "todo".to_owned(),
        ordinal: None,
        pr_url: pr.map(str::to_owned),
        deleted_at: None,
        created_at: ts.to_owned(),
        updated_at: ts.to_owned(),
        autostart: true,
        last_status_actor: "human".to_owned(),
        priority: "medium".to_owned(),
        created_via: "cli".to_owned(),
        repo_remote_url: None,
        blocked_reason: None,
        blocked_attempt_id: None,
        blocked_signals: vec![],
        effort_level: None,
        model_override: None,
        ci_attempt_budget: None,
        ci_attempts_used: 0,
        ci_required_state: None,
        ci_required_detail: None,
        review_required_state: None,
        review_required_detail: None,
        pr_state_polled_at: None,
        merge_queue_state: None,
        external_ref: None,
        parent_task_id: parent.map(str::to_owned),
        revision_seq: None,
        revision_parent_pr_url: None,
        has_in_progress_revision: false,
        source_automation_id: None,
    }
}

mod t01;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
