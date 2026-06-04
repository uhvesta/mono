use super::*;

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
        .sync_project_design_doc_from_detector(&project.id, None, None, "/absolute/path.md")
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
    assert_eq!(audit.len(), 6, "3 set + 3 unset = 6 rows, got: {audit:#?}",);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
                        pr_url: Some(format!("https://github.com/spinyfin/mono/pull/{}", 100 + i)),
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
    let (_path, db, _product_id, task_id) = make_resolve_scaffold("resolve-both-null", None);
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

/// Design tasks on a product with `design_repo` set must resolve
/// to `design_repo` rather than `repo_remote_url`. Acceptance
/// criterion: "`boss task show --json` for a `kind=design` task
/// on that product resolves to `design_repo` for its repo,
/// without any task-level `--repo` set."
#[test]
fn resolve_repo_uses_design_repo_for_design_kind() {
    let path = disk_db_path("resolve-design-repo");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: Some("git@github.com:linkedin-sandbox/bduff.git".to_owned()),
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    // Project creation seeds a `kind = 'design'` task.
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

    // Find the seed design task (ordinal = 0).
    let conn = db.connect().unwrap();
    let design_task_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();

    let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
        "design task must resolve to product.design_repo",
    );

    // Implementation-kind tasks on the same product are unaffected.
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "Implementation chore".to_owned(),
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
    let resolved = resolve_repo_for_work_item(&conn, &chore.id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "implementation-kind tasks must continue to resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// `design_repo` falls through to `repo_remote_url` when unset.
/// Pre-existing products (and the explicit None path) behave
/// exactly as before.
#[test]
fn resolve_repo_design_kind_without_design_repo_falls_through_to_product_repo() {
    let path = disk_db_path("resolve-design-no-override");
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
            name: "Project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    let design_task_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "without design_repo, a design task must resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Task-level `--repo` still wins over `design_repo`. The new
/// override slots in as a new middle layer; it does not change
/// the priority of per-row overrides above it. To plant a
/// row-level override on a single-repo product (the typical case
/// when `design_repo` is set), the test bypasses the
/// task-creation invariant and writes the column directly.
#[test]
fn resolve_repo_task_override_wins_over_design_repo() {
    let path = disk_db_path("resolve-design-override-wins");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: Some("git@github.com:linkedin-sandbox/bduff.git".to_owned()),
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
    let conn = db.connect().unwrap();
    let design_task_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
        params![design_task_id, "git@github.com:custom/elsewhere.git"],
    )
    .unwrap();

    let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:custom/elsewhere.git"),
        "row-level override must win over product.design_repo",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Round-trip: setting `design_repo` via `create` and clearing it
/// via `update_work_item("")` mirrors the wire-level behaviour of
/// `repo_remote_url`. Confirms the patch path applies / clears
/// the column rather than silently ignoring it.
#[test]
fn product_design_repo_set_and_clear() {
    let path = disk_db_path("design-repo-set-clear");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: Some("git@github.com:linkedin-sandbox/bduff.git".to_owned()),
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    assert_eq!(
        product.design_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
    );

    let cleared = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                design_repo: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(cleared) = cleared else {
        panic!("expected Product");
    };
    assert!(
        cleared.design_repo.is_none(),
        "empty-string patch must clear design_repo, got {:?}",
        cleared.design_repo,
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Investigation tasks on a product with `docs_repo` set must
/// resolve to `docs_repo` rather than `repo_remote_url`, the
/// docs-repo analogue of the `design_repo` routing above.
/// Implementation-kind tasks on the same product are unaffected.
#[test]
fn resolve_repo_uses_docs_repo_for_investigation_kind() {
    let path = disk_db_path("resolve-docs-repo");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: Some("git@github.com:linkedin-sandbox/bduff.git".to_owned()),
            worker_branch_prefix: None,
        })
        .unwrap();

    // Create a chore, then flip its kind to `investigation` directly
    // (bypassing the create invariant) so the resolver sees an
    // investigation row on a single-repo product.
    let investigation = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "Investigation".to_owned(),
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
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET kind = 'investigation' WHERE id = ?1",
        [&investigation.id],
    )
    .unwrap();

    let resolved = resolve_repo_for_work_item(&conn, &investigation.id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
        "investigation task must resolve to product.docs_repo",
    );

    // Implementation-kind tasks on the same product are unaffected.
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "Implementation chore".to_owned(),
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
    let resolved = resolve_repo_for_work_item(&conn, &chore.id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "implementation-kind tasks must continue to resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Round-trip: setting `docs_repo` via `create` and clearing it via
/// `update_work_item("")` mirrors the `design_repo` behaviour.
/// Confirms the patch path applies / clears the column rather than
/// silently ignoring it.
#[test]
fn product_docs_repo_set_and_clear() {
    let path = disk_db_path("docs-repo-set-clear");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: Some("git@github.com:linkedin-sandbox/bduff.git".to_owned()),
            worker_branch_prefix: None,
        })
        .unwrap();
    assert_eq!(
        product.docs_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
    );

    // Updating an unrelated field must leave docs_repo intact.
    let renamed = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                name: Some("Boss Renamed".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(renamed) = renamed else {
        panic!("expected Product");
    };
    assert_eq!(
        renamed.docs_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
        "patch that omits docs_repo must leave it unchanged",
    );

    let cleared = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                docs_repo: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(cleared) = cleared else {
        panic!("expected Product");
    };
    assert!(
        cleared.docs_repo.is_none(),
        "empty-string patch must clear docs_repo, got {:?}",
        cleared.docs_repo,
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

    let cleared = db.set_product_default_model(&product.id, Some("")).unwrap();
    assert!(cleared.default_model.is_none());

    let cleared_again = db.set_product_default_model(&product.id, None).unwrap();
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let product = db
        .create_product(CreateProductInput {
            name: "Foo".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
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
        let val: Option<String> = conn2
            .query_row(
                "SELECT repo_remote_url FROM tasks WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            val.is_none(),
            "mirrored chore {id} must be NULL after migration, got {val:?}"
        );
    }
    let mirrored_task_val: Option<String> = conn2
        .query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&mirrored_task_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        mirrored_task_val.is_none(),
        "mirrored task must be NULL after migration, got {mirrored_task_val:?}"
    );

    // The divergent override must remain unchanged.
    let divergent_val: Option<String> = conn2
        .query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&divergent_id],
            |row| row.get(0),
        )
        .unwrap();
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .prepare("SELECT short_id FROM tasks WHERE product_id = ?1 AND short_id IS NOT NULL")
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let flunge = db
        .create_product(CreateProductInput {
            name: "Flunge".into(),
            description: None,
            repo_remote_url: Some("git@example.com:flunge.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        conn.query_row("SELECT short_id FROM tasks WHERE id = ?1", [id], |row| {
            row.get(0)
        })
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
            ("tasks", "task_a", "chore", 100),
            ("tasks", "task_b", "chore", 100),
            ("projects", "proj_a", "", 200),
            ("tasks", "task_c", "chore", 300),
            ("projects", "proj_b", "", 400),
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
        conn.execute(
            "DELETE FROM short_id_sequences WHERE product_id = 'prod_a'",
            [],
        )
        .unwrap();
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
    assert_eq!(
        run_a, run_b,
        "two independent runs must produce identical short_ids"
    );

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert_eq!(
        chore.short_id,
        Some(1),
        "first chore in product gets short_id 1"
    );

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
    let fetched = db.list_chores(&product.id, None, false).unwrap();
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let current_status = project.status.to_string();
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

    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET last_status_actor = 'engine' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    }

    // Genuine status change: todo → active.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
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

/// `list_ci_remediations` honours the `(product, status, work_item)`
/// filter triple AND-ed and orders rows freshest-first. The empty
/// filter set returns every row; `status = []` matches every
/// status.
#[test]
fn list_ci_remediations_filters_and_orders_freshest_first() {
    let path = disk_db_path("list-ci-remediations-filters");
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
    let chore_a = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-a".into(),
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
    let chore_b = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore-b".into(),
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
    // Two rows for chore_a with different attempt_kinds + statuses.
    let r1 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_a.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/100".into(),
            pr_number: 100,
            head_branch: "feature-a".into(),
            head_sha_at_trigger: "head-a-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    let r2 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_a.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/100".into(),
            pr_number: 100,
            head_branch: "feature-a".into(),
            head_sha_at_trigger: "head-a-2".into(),
            attempt_kind: "retrigger".into(),
            consumes_budget: 0,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    // One row for chore_b.
    let r3 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_b.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/101".into(),
            pr_number: 101,
            head_branch: "feature-b".into(),
            head_sha_at_trigger: "head-b-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    db.mark_ci_remediation_failed(&r1.id, "boom").unwrap();

    // No filters → every row, freshest first.
    let all = db.list_ci_remediations(None, &[], None, None).unwrap();
    assert_eq!(all.len(), 3);
    // Most-recently-inserted row should be first.
    assert_eq!(all[0].id, r3.id);

    // Filter by product.
    let by_product = db
        .list_ci_remediations(Some(&product.id), &[], None, None)
        .unwrap();
    assert_eq!(by_product.len(), 3);

    // Filter by work item.
    let by_item = db
        .list_ci_remediations(None, &[], Some(&chore_a.id), None)
        .unwrap();
    assert_eq!(by_item.len(), 2);
    for row in &by_item {
        assert_eq!(row.work_item_id, chore_a.id);
    }

    // Filter by status: `failed` matches only r1.
    let failed_rows = db
        .list_ci_remediations(None, &["failed".into()], None, None)
        .unwrap();
    assert_eq!(failed_rows.len(), 1);
    assert_eq!(failed_rows[0].id, r1.id);

    // Limit caps the row set.
    let capped = db.list_ci_remediations(None, &[], None, Some(2)).unwrap();
    assert_eq!(capped.len(), 2);

    // Compound: product + work_item + status, intersected.
    let intersect = db
        .list_ci_remediations(
            Some(&product.id),
            &["pending".into()],
            Some(&chore_a.id),
            None,
        )
        .unwrap();
    assert_eq!(intersect.len(), 1);
    assert_eq!(intersect[0].id, r2.id);

    let _ = std::fs::remove_file(path);
}

/// `ci_budget_snapshot` joins `tasks.ci_attempt_budget` with the
/// product's `ci_attempt_budget` to produce the effective budget,
/// reads `ci_attempts_used`, and clamps the effective value to
/// `0..=10`. `blocked_reason` is reported only when the task is
/// currently `status='blocked'`.
#[test]
fn ci_budget_snapshot_combines_override_and_product_default() {
    let path = disk_db_path("ci-budget-snapshot");
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
            name: "chore-budget".into(),
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

    // Defaults: no per-PR override, product default = 3, used = 0.
    let snap = db.ci_budget_snapshot(&chore.id).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, None);
    assert_eq!(snap.product_default, 3);
    assert_eq!(snap.effective, 3);
    assert_eq!(snap.used, 0);
    assert_eq!(snap.blocked_reason, None);

    // Override path: `set_ci_attempt_budget` clamps to `0..=10`.
    let snap = db
        .set_ci_attempt_budget(&chore.id, Some(7))
        .unwrap()
        .unwrap();
    assert_eq!(snap.per_pr_override, Some(7));
    assert_eq!(snap.effective, 7);
    // Out-of-range value clamps.
    let snap = db
        .set_ci_attempt_budget(&chore.id, Some(25))
        .unwrap()
        .unwrap();
    assert_eq!(snap.per_pr_override, Some(10));
    assert_eq!(snap.effective, 10);
    // Clear path → product default applies.
    let snap = db.set_ci_attempt_budget(&chore.id, None).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, None);
    assert_eq!(snap.effective, 3);

    // Unknown work item.
    assert!(
        db.ci_budget_snapshot("chr_does_not_exist")
            .unwrap()
            .is_none()
    );

    let _ = std::fs::remove_file(path);
}

/// Regression: rows with `effort_level = ''` (empty string, produced by
/// older write paths when clearing the field) should be converted to NULL
/// by the `migrate_tasks_empty_effort_to_null` migration so canonical
/// DB storage matches the schema intent and SQL `IS NULL` queries remain
/// reliable.
#[test]
fn migration_normalises_empty_effort_level_to_null() {
    let path = disk_db_path("effort-empty-to-null");
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
            name: "Chore with empty effort".into(),
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

    // Manually write an empty string to simulate a legacy row.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET effort_level = '' WHERE id = ?1",
            [&chore.id],
        )
        .unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT effort_level FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw.as_deref(), Some(""), "pre-condition: row has ''");
    }
    drop(db);

    // Re-opening runs the migration which converts '' to NULL.
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    let after: Option<String> = conn
        .query_row(
            "SELECT effort_level FROM tasks WHERE id = ?1",
            [&chore.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(after.is_none(), "empty effort_level should be NULL after migration");

    let _ = std::fs::remove_file(path);
}

/// `mark_ci_remediation_retriggered` flips the attempt to the terminal
/// `retriggered` status, records the flaky verdict, and stamps the
/// `ci_flaky_retriggered` signal on the parent WITHOUT moving it to
/// `status='blocked'`. The signal is what the completion path consults to
/// park the worker instead of looping. Idempotent on a re-marker.
#[test]
fn mark_ci_remediation_retriggered_records_flaky_signal_without_blocking() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "flaky");
    let pr = "https://github.com/spinyfin/mono/pull/71";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 71,
            head_branch: "boss/exec".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "retrigger".into(),
            consumes_budget: 0,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");

    // No flaky signal before the marker.
    assert!(!db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());

    let updated = db
        .mark_ci_remediation_retriggered(&attempt.id)
        .unwrap()
        .expect("retrigger flip");
    assert_eq!(updated.status, "retriggered");
    assert_eq!(updated.triage_class.as_deref(), Some("flaky_or_infra"));
    assert!(updated.finished_at.is_some());

    // Signal is active and FK-linked to the attempt.
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    let signals = db.active_blocked_signals(&chore).unwrap();
    let flaky = signals
        .iter()
        .find(|s| s.reason == "ci_flaky_retriggered")
        .expect("flaky signal present");
    assert_eq!(flaky.attempt_id.as_deref(), Some(attempt.id.as_str()));

    // The parent is NOT moved to blocked — it stays in_review.
    let task = match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert!(task.blocked_reason.is_none());

    // The attempt is now terminal, so `active_ci_remediation_for_work_item`
    // (pending/running only) no longer returns it — the on-Stop catch-all
    // finalizer becomes a no-op and cannot re-mark it failed.
    assert!(db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .is_none());

    // Idempotent: a duplicate marker is a no-op (row already terminal).
    assert!(db.mark_ci_remediation_retriggered(&attempt.id).unwrap().is_none());
    assert_eq!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .filter(|s| s.reason == "ci_flaky_retriggered")
            .count(),
        1,
        "duplicate marker must not double-arm the signal",
    );
}

/// The `ci_flaky_retriggered` signal is cleared both when CI resolves
/// (`clear_ci_failure_signal_only`) and when a fresh remediation attempt
/// supersedes the verdict (`insert_ci_remediation`).
#[test]
fn ci_flaky_retrigger_signal_clears_on_resolve_and_supersede() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "flaky-clear");
    let pr = "https://github.com/spinyfin/mono/pull/72";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let arm = |head: &str| -> String {
        let a = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product_id.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 72,
                head_branch: "boss/exec".into(),
                head_sha_at_trigger: head.into(),
                attempt_kind: "retrigger".into(),
                consumes_budget: 0,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .expect("insert");
        db.mark_ci_remediation_retriggered(&a.id).unwrap().expect("flip");
        a.id
    };

    // Path 1: CI resolves while the parent stayed in_review.
    arm("head-a");
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    assert!(db.clear_ci_failure_signal_only(&chore).unwrap());
    assert!(!db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());

    // Path 2: a fresh remediation attempt supersedes the stale verdict.
    arm("head-b");
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product_id.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 72,
        head_branch: "boss/exec".into(),
        head_sha_at_trigger: "head-c".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap()
    .expect("fresh insert");
    assert!(
        !db.has_active_ci_flaky_retrigger_signal(&chore).unwrap(),
        "a new remediation attempt must supersede the stale flaky verdict",
    );
}

/// Helper: collect the `reason` of every active blocked signal for a
/// work item (cleared signals are excluded by `active_blocked_signals`).
#[cfg(test)]
fn active_signal_reasons(db: &WorkDb, work_item_id: &str) -> Vec<String> {
    db.active_blocked_signals(work_item_id)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect()
}

/// Helper: read a chore/task work item and unwrap to its `Task`.
#[cfg(test)]
fn task_of(db: &WorkDb, work_item_id: &str) -> Task {
    match db.get_work_item(work_item_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected a chore/task work item, got {other:?}"),
    }
}

/// `effective_ci_budget` resolves the per-PR override first, falls back
/// to the parent product's default, returns the hard default `3` for an
/// unknown work item, and clamps the resolved value to `0..=10`.
///
/// The unknown-item branch deliberately diverges from
/// `ci_budget_snapshot` (which returns `None`); this test pins both
/// behaviours side by side so the divergence is intentional, not drift.
#[test]
fn effective_ci_budget_resolves_override_default_and_clamps() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "eff-budget");
    let pr = "https://github.com/spinyfin/mono/pull/900";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // (b) No per-PR override → the product default applies. Use a value
    // that is NOT the documented hard default of 3, so a regression that
    // ignores the product column and hard-codes 3 would be caught.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE products SET ci_attempt_budget = 5 WHERE id = ?1",
            [&product_id],
        )
        .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        5,
        "falls back to the product default when no per-PR override is set",
    );

    // (a) A per-PR override wins over the product default.
    db.set_ci_attempt_budget(&chore, Some(7))
        .unwrap()
        .expect("override write");
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        7,
        "the per-PR override takes precedence over the product default",
    );

    // (d) The resolved value is clamped to `0..=10`. `set_ci_attempt_budget`
    // already clamps on write, so poke the raw column past the bounds to
    // exercise the read-side clamp inside `effective_ci_budget` itself.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET ci_attempt_budget = 99 WHERE id = ?1",
            [&chore],
        )
        .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        10,
        "an over-cap override clamps up to the hard ceiling of 10",
    );
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET ci_attempt_budget = -4 WHERE id = ?1",
            [&chore],
        )
        .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        0,
        "a negative override clamps up to the floor of 0",
    );

    // (c) Unknown work item → the hard default of 3, which diverges from
    // `ci_budget_snapshot` returning `None` for the same missing id.
    assert_eq!(
        db.effective_ci_budget("chr_does_not_exist").unwrap(),
        3,
        "an unknown work item returns the hard default budget of 3",
    );
    assert!(
        db.ci_budget_snapshot("chr_does_not_exist")
            .unwrap()
            .is_none(),
        "ci_budget_snapshot diverges: it returns None for the same unknown item",
    );
}

/// Full CI-failure block lifecycle through the public API:
/// `mark_chore_blocked_ci_failure` blocks the parent and arms the
/// `ci_failure` signal; `clear_chore_blocked_ci_failure` flips it back
/// to `in_review` and clears the signal; and
/// `rearm_blocked_ci_failure_signal` reactivates a signal that was
/// cleared out from under a still-blocked parent (while staying a no-op
/// when the parent is no longer blocked). Assertions observe the public
/// task status / active-signal projection, never internal columns.
#[test]
fn ci_failure_block_mark_clear_rearm_lifecycle() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "ci-block-lifecycle");
    let pr = "https://github.com/spinyfin/mono/pull/901";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // mark: in_review → blocked: ci_failure, with the signal armed.
    db.mark_chore_blocked_ci_failure(&chore, pr, None)
        .unwrap()
        .expect("flip to blocked: ci_failure");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::Blocked);
    assert_eq!(t.blocked_reason.as_deref(), Some("ci_failure"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);

    // clear: blocked → in_review, and the active signal is cleared too.
    db.clear_chore_blocked_ci_failure(&chore, pr)
        .unwrap()
        .expect("clear the ci_failure block");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.blocked_reason.is_none());
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "clearing the block also clears the active ci_failure signal",
    );

    // rearm is a no-op while the parent is not blocked.
    assert!(
        !db.rearm_blocked_ci_failure_signal(&chore).unwrap(),
        "rearm must not arm a signal on a parent that is no longer blocked",
    );
    assert!(active_signal_reasons(&db, &chore).is_empty());

    // Re-block, then simulate a premature polymorphic clear that drops the
    // signal row but leaves the parent blocked. rearm must reactivate it.
    db.mark_chore_blocked_ci_failure(&chore, pr, None)
        .unwrap()
        .expect("re-block: ci_failure");
    assert!(
        db.clear_ci_failure_signal_only(&chore).unwrap(),
        "signal-only clear deactivates the signal",
    );
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the signal is inactive after the signal-only clear",
    );
    assert_eq!(
        task_of(&db, &chore).status,
        TaskStatus::Blocked,
        "the parent stays blocked through a signal-only clear",
    );
    assert!(
        db.rearm_blocked_ci_failure_signal(&chore).unwrap(),
        "rearm reactivates a cleared signal while the parent is still blocked",
    );
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);
}

/// Merge-conflict counterpart of the CI-failure lifecycle test, exercising
/// `clear_chore_blocked_merge_conflict` and
/// `rearm_blocked_merge_conflict_signal` (both live, previously untested):
/// mark → clear (block + signal) → no-op rearm when in_review →
/// re-block → signal-only clear → rearm reactivates.
#[test]
fn merge_conflict_block_mark_clear_rearm_lifecycle() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "mc-block-lifecycle");
    let pr = "https://github.com/spinyfin/mono/pull/902";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // mark: in_review → blocked: merge_conflict, with the signal armed.
    db.mark_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("flip to blocked: merge_conflict");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::Blocked);
    assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);

    // clear: blocked → in_review, and the active signal is cleared too.
    db.clear_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("clear the merge_conflict block");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.blocked_reason.is_none());
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "clearing the block also clears the active merge_conflict signal",
    );

    // rearm is a no-op while the parent is not blocked.
    assert!(
        !db.rearm_blocked_merge_conflict_signal(&chore).unwrap(),
        "rearm must not arm a signal on a parent that is no longer blocked",
    );
    assert!(active_signal_reasons(&db, &chore).is_empty());

    // Re-block, drop only the signal (premature polymorphic clear), then
    // confirm rearm reactivates it while the parent stays blocked.
    db.mark_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("re-block: merge_conflict");
    assert!(
        db.clear_merge_conflict_signal_only(&chore).unwrap(),
        "signal-only clear deactivates the signal",
    );
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the signal is inactive after the signal-only clear",
    );
    assert_eq!(
        task_of(&db, &chore).status,
        TaskStatus::Blocked,
        "the parent stays blocked through a signal-only clear",
    );
    assert!(
        db.rearm_blocked_merge_conflict_signal(&chore).unwrap(),
        "rearm reactivates a cleared signal while the parent is still blocked",
    );
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);
}
