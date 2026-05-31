use super::*;

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        dependent_execs.iter().all(|exec| exec.status != "ready"),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind("chore_implementation")
            .status("ready")
            .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind("chore_implementation")
            .status("ready")
            .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind("chore_implementation")
            .status("ready")
            .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let p2 = db
        .create_product(CreateProductInput {
            name: "Beta".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/beta.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

/// Auto-unblock creates a `ready` execution atomically. After a
/// prereq goes `done` and the cascade flips a dependent to `todo`,
/// the dependent must have a `ready` execution so the coordinator
/// can dispatch it on the next kick without a separate reconcile call.
#[test]
fn auto_unblock_creates_ready_execution() {
    let path = temp_db_path("auto-unblock-creates-ready-exec");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let dep = db
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
    db.add_dependency(AddDependencyInput {
        dependent: dep.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();

    // dep is now blocked; mark prereq done via the normal cascade path.
    db.update_work_item(
        &prereq.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let dep_after = db.get_work_item(&dep.id).unwrap();
    let WorkItem::Chore(t) = dep_after else {
        panic!()
    };
    assert_eq!(t.status, "todo", "dependent must be unblocked to todo");

    // Key assertion: the execution must be `ready` so the coordinator
    // can dispatch it on the next kick — no external reconcile needed.
    let executions = db.list_executions(Some(&dep.id)).unwrap();
    assert_eq!(executions.len(), 1, "must have exactly one execution");
    assert_eq!(
        executions[0].status, "ready",
        "auto-unblock must promote execution to ready so coordinator can dispatch"
    );
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let WorkItem::Chore(t) = blocked else {
        panic!()
    };
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
    let WorkItem::Chore(t) = still_blocked else {
        panic!()
    };
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
    let WorkItem::Chore(t) = unblocked else {
        panic!()
    };
    assert_eq!(
        t.status, "todo",
        "all prereqs done — dependent must auto-unblock"
    );
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let WorkItem::Chore(t) = unblocked else {
        panic!()
    };
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
    let WorkItem::Chore(t) = after_regression else {
        panic!()
    };
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
        gating,
        [prereq.id.clone()],
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let updated = db
        .mark_chore_pr_merged(&b.id, "https://example.test/pr/1")
        .unwrap();
    assert!(
        updated.is_some(),
        "mark_chore_pr_merged should report a transition for the cycle prereq",
    );
    let b_after = db.get_work_item(&b.id).unwrap();
    let WorkItem::Chore(t) = b_after else {
        panic!()
    };
    assert_eq!(t.status, "done");
    let _ = std::fs::remove_file(path);
}

/// T701-class deadlock: a `kind = 'revision'` task gated on its parent
/// via a `blocks` edge must unblock as soon as the parent reaches
/// `in_review` (PR open). Previously the cascade bailed out on
/// `in_review` (only `done` was considered satisfying) so the revision
/// could never start — the PR would be merged and the revision's window
/// gone by the time it cleared its gate.
///
/// Also verifies the negative: a non-revision (chore) dependent gated
/// on the same parent must NOT unblock on `in_review`; it still requires
/// `done`.
#[test]
fn revision_unblocks_when_prereq_reaches_in_review() {
    let db = WorkDb::open(temp_db_path("revision-unblock-in-review")).unwrap();
    let product_id = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap()
        .id;

    // Parent chore — will transition to in_review.
    let parent = db
        .create_chore(CreateChoreInput {
            product_id: product_id.clone(),
            name: "Parent chore".to_owned(),
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

    // Revision task — should unblock on parent reaching `in_review`.
    // Insert directly to bypass the PR-state gate on `create_revision`.
    let revision_id = {
        let conn = db.connect().unwrap();
        let id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks
                    (id, product_id, kind, name, description, status, autostart,
                     last_status_actor, created_at, updated_at, parent_task_id)
                 VALUES (?1, ?2, 'revision', 'Revision', '', 'todo', 1, 'engine', ?3, ?3, ?4)",
            rusqlite::params![id, product_id, now, parent.id],
        )
        .unwrap();
        id
    };

    // Non-revision (chore) dependent — must NOT unblock on in_review.
    let chore_dep = db
        .create_chore(CreateChoreInput {
            product_id: product_id.clone(),
            name: "Chore dependent".to_owned(),
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

    // Gate both dependents on the parent.
    db.add_dependency(AddDependencyInput {
        dependent: revision_id.clone(),
        prerequisite: parent.id.clone(),
        relation: None,
    })
    .unwrap();
    db.add_dependency(AddDependencyInput {
        dependent: chore_dep.id.clone(),
        prerequisite: parent.id.clone(),
        relation: None,
    })
    .unwrap();

    // Verify both are now blocked (auto-blocked by add_dependency).
    let rev_before = db.get_work_item(&revision_id).unwrap();
    // Revision kind maps to WorkItem::Task (non-chore kinds are Task).
    let WorkItem::Task(rev_t) = rev_before else {
        panic!("expected WorkItem::Task for revision")
    };
    assert_eq!(
        rev_t.status, "blocked",
        "revision must be auto-blocked after add_dependency"
    );

    let chore_before = db.get_work_item(&chore_dep.id).unwrap();
    let WorkItem::Chore(chore_t) = chore_before else {
        panic!()
    };
    assert_eq!(
        chore_t.status, "blocked",
        "chore dependent must be auto-blocked after add_dependency"
    );

    // Transition parent to `in_review` (simulates PR opened).
    db.update_work_item(
        &parent.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Revision must now be unblocked (gate satisfied by in_review).
    let rev_after = db.get_work_item(&revision_id).unwrap();
    let WorkItem::Task(rev_a) = rev_after else {
        panic!()
    };
    assert_eq!(
        rev_a.status, "todo",
        "revision must unblock when prereq reaches in_review",
    );
    assert!(
        rev_a.blocked_reason.is_none(),
        "blocked_reason must be cleared on unblock"
    );

    // Chore dependent must still be blocked — in_review does not satisfy it.
    let chore_after = db.get_work_item(&chore_dep.id).unwrap();
    let WorkItem::Chore(chore_a) = chore_after else {
        panic!()
    };
    assert_eq!(
        chore_a.status, "blocked",
        "non-revision chore must remain blocked when prereq is only in_review",
    );

    // Sanity: parent reaching `done` must then unblock the chore dep too.
    db.update_work_item(
        &parent.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let chore_done = db.get_work_item(&chore_dep.id).unwrap();
    let WorkItem::Chore(chore_d) = chore_done else {
        panic!()
    };
    assert_eq!(
        chore_d.status, "todo",
        "chore must unblock when prereq reaches done",
    );
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .request_execution(RequestExecutionInput::builder()
            .work_item_id(a.id.clone())
            .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let WorkItem::Chore(t) = dep_after_add else {
        panic!()
    };
    assert_eq!(
        t.status, "blocked",
        "engine should have auto-blocked dependent"
    );
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
    let WorkItem::Chore(stuck) = still_stuck else {
        panic!()
    };
    assert_eq!(stuck.status, "blocked", "cascade skipped — still stuck");
    assert_eq!(stuck.blocked_reason, None);

    // RequestExecution (the user-override path) must succeed and
    // clear the stale block.
    let execution = db
        .request_execution(RequestExecutionInput::builder()
            .work_item_id(dependent.id.clone())
            .build())
        .expect("RequestExecution should succeed when all prereqs are done");

    assert_eq!(execution.status, "ready", "execution must be ready");

    // The task's kanban status must be cleared to 'todo' so
    // start_execution_run can advance it to 'active'.
    let dep_final = db.get_work_item(&dependent.id).unwrap();
    let WorkItem::Chore(final_task) = dep_final else {
        panic!()
    };
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
    assert_eq!(version, "14");
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let project_tasks = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap();
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
    assert_eq!(version, "14");
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
    assert_eq!(version, "14");

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
    assert_eq!(version, "14");

    let _ = std::fs::remove_file(path);
}
