use super::*;

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore_a.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore_b.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Ready)
        .build())
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
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore_c.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Completed)
        .cube_lease_id("lease-C")
        .cube_workspace_id("mono-agent-002")
        .workspace_path("/tmp/mono-agent-002")
        .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(live.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(dead.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(unknown.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
    let skip_dispatch: HashSet<String> = [exec_live.id.clone(), exec_unknown.id.clone()]
        .into_iter()
        .collect();

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
    assert_eq!(live_after[0].status, ExecutionStatus::Running);
    assert_eq!(live_after[0].cube_lease_id.as_deref(), Some("lease-LIVE"));

    // Unknown row: also unchanged. The probe didn't know either
    // way, so we MUST NOT redispatch — that's the conservatism
    // the work-item brief insists on ("ambiguous → leave alone").
    let unknown_after = db.list_executions(Some(&unknown.id)).unwrap();
    assert_eq!(
        unknown_after.len(),
        1,
        "unknown execution row must be preserved"
    );
    assert_eq!(unknown_after[0].status, ExecutionStatus::Running);
    assert_eq!(unknown_after[0].cube_lease_id.as_deref(), Some("lease-UNK"));

    // Dead row: original abandoned, fresh `ready` row inserted
    // alongside it. The dispatcher will pick up the new row on
    // its next tick.
    let dead_after = db.list_executions(Some(&dead.id)).unwrap();
    assert_eq!(
        dead_after.len(),
        2,
        "dead row gets a redispatch alongside the abandonment"
    );
    let original = dead_after.iter().find(|e| e.id == exec_dead.id).unwrap();
    assert_eq!(original.status, ExecutionStatus::Abandoned);
    assert!(original.finished_at.is_some());
    let fresh = dead_after.iter().find(|e| e.id != exec_dead.id).unwrap();
    assert_eq!(fresh.status, ExecutionStatus::Ready);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::WaitingHuman)
            .build())
        .unwrap();

    let new_exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_ne!(new_exec.id, stale.id, "expected a brand new execution row");
    assert_eq!(new_exec.status, ExecutionStatus::Ready);

    let stale_after = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|e| e.id == stale.id)
        .unwrap();
    assert_eq!(stale_after.status, ExecutionStatus::Abandoned);
    assert!(stale_after.finished_at.is_some());
}

/// When drag-to-Doing fires for a chore whose latest non-terminal
/// execution is a stale (no live worker) `ci_remediation`, the engine
/// must re-queue that execution (flip `waiting_human` → `ready`) rather
/// than abandoning it and spawning a fresh `chore_implementation`. The
/// existing execution row carries `pr_head_before` and the old workspace
/// context; re-queuing it routes the new worker back to the existing
/// branch/PR rather than re-implementing from scratch.
///
/// The drag-to-Doing case: task is already `active` before
/// `request_execution` is called (the UI sets status first).
#[test]
fn request_execution_requeues_stale_ci_remediation_drag_to_doing() {
    let path = temp_db_path("req-ci-remediation-drag");
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
            name: "CI-failing chore".to_owned(),
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
    // Simulate: chore is active (UI already dragged to Doing),
    // and a previous ci_remediation worker ran but is now gone.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let ci_exec = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::CiRemediation)
            .status(ExecutionStatus::WaitingHuman)
            .cube_workspace_id("mono-agent-001")
            .cube_lease_id("lease-ci-old")
            .workspace_path("/ws/mono-agent-001")
            .build())
        .unwrap();

    let requeued = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(
        requeued.id, ci_exec.id,
        "must reuse the ci_remediation row, not create a new one"
    );
    assert_eq!(requeued.status, ExecutionStatus::Ready);
    assert_eq!(requeued.kind, ExecutionKind::CiRemediation);
    assert_eq!(
        requeued.preferred_workspace_id.as_deref(),
        Some("mono-agent-001"),
        "old workspace promoted to preferred so dispatcher can re-claim it",
    );
    assert!(
        requeued.cube_lease_id.is_none(),
        "stale lease must be cleared"
    );
    assert!(
        requeued.cube_workspace_id.is_none(),
        "stale workspace columns cleared"
    );

    let all = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(all.len(), 1, "no new execution row inserted");
}

/// Same ci_remediation re-queue semantics via `bossctl work start`:
/// the task is still `status='blocked'` (ci_failure) when
/// `request_execution` is called (the CLI does not flip kanban status
/// first). The engine must clear the ci_failure block so
/// `start_execution_run` can advance the task to `active`, and must
/// insert a ci_failure_suppression so the CI watch does not immediately
/// re-flip the task before the worker pushes a fix.
#[test]
fn request_execution_requeues_ci_remediation_from_blocked_bossctl_path() {
    let path = temp_db_path("req-ci-remediation-bossctl");
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
            name: "CI blocked chore".to_owned(),
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
    let pr_url = "https://github.com/spinyfin/mono/pull/686".to_owned();
    // Move to in_review so mark_chore_blocked_ci_failure accepts.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    // Insert a ci_remediations row so the suppression can key on a head sha.
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore.id.clone(),
        pr_url: pr_url.clone(),
        pr_number: 686,
        head_branch: "boss/exec_18b0bee86849d850_11".into(),
        head_sha_at_trigger: "sha-abc123".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap();
    // Flip to blocked: ci_failure (marks task.status='blocked').
    db.mark_chore_blocked_ci_failure(&chore.id, &pr_url, None)
        .unwrap();
    let task_before = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected Task/Chore, got {other:?}"),
    };
    assert_eq!(task_before.status, TaskStatus::Blocked);
    assert_eq!(task_before.blocked_reason.as_deref(), Some("ci_failure"));

    // ci_remediation execution was created and ran, but worker is now gone.
    let ci_exec = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::CiRemediation)
            .status(ExecutionStatus::WaitingHuman)
            .cube_workspace_id("mono-agent-001")
            .cube_lease_id("lease-ci-old")
            .workspace_path("/ws/mono-agent-001")
            .build())
        .unwrap();

    let requeued = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(requeued.id, ci_exec.id, "must reuse ci_remediation row");
    assert_eq!(requeued.status, ExecutionStatus::Ready);
    assert_eq!(requeued.kind, ExecutionKind::CiRemediation);
    assert_eq!(
        requeued.preferred_workspace_id.as_deref(),
        Some("mono-agent-001"),
    );
    assert!(requeued.cube_lease_id.is_none());

    // Task must have been unblocked so start_execution_run can advance it.
    let task_after = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected Task/Chore, got {other:?}"),
    };
    assert_eq!(
        task_after.status, TaskStatus::Todo,
        "ci_failure block cleared to todo"
    );
    assert!(task_after.blocked_reason.is_none());

    // Suppression row must exist so the CI watch stays quiet.
    assert!(
        db.is_ci_failure_suppressed(&chore.id, "sha-abc123")
            .unwrap(),
        "suppression row must be keyed on the ci_remediations head sha",
    );
}

/// Re-dispatch storm guard (`task_18b347260cd7da80_e`). A work item
/// whose genuinely-live `running` execution is shadowed by a NEWER
/// terminal execution (a prior re-dispatch that stalled and was
/// orphaned) must NOT get a fresh dispatch. The idempotency check
/// must key off the live execution, not the latest-by-created_at
/// row — otherwise every sweep spawns another duplicate that claims
/// a worker and stalls.
#[test]
fn request_execution_suppressed_when_older_execution_is_live() {
    let path = temp_db_path("redispatch-storm-suppress");
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
            name: "R693".to_owned(),
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

    // The live run (La Forge): created first, so it is the OLDER row.
    let live = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build())
        .unwrap();
    // A stalled re-dispatch that was orphaned: NEWER, terminal,
    // shadows `live` as the latest-by-created_at row.
    let phantom = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Orphaned)
            .build())
        .unwrap();
    // Sanity: the phantom is the latest row (the trap).
    assert_eq!(
        query_latest_execution_for_work_item(&db.connect().unwrap(), &chore.id)
            .unwrap()
            .unwrap()
            .id,
        phantom.id,
        "test setup: newer terminal execution must shadow the live one",
    );

    // The live execution is genuinely claimed by a worker.
    let result = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
            |exec_id| exec_id == live.id,
        )
        .unwrap();

    assert_eq!(
        result.id, live.id,
        "must return the live execution, not spawn a duplicate",
    );
    assert_eq!(result.status, ExecutionStatus::Running);
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1",
            [&chore.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "no new execution may be created");
    let _ = std::fs::remove_file(path);
}

/// Counterpart to the storm-suppression test: when the live-status
/// execution is NOT actually claimed by a worker (the worker died
/// with the DB still saying `running`), the dead row must be
/// abandoned and a fresh `ready` execution created. This is the
/// legitimate re-dispatch path the storm guard must not break.
#[test]
fn request_execution_redispatches_when_live_execution_not_claimed() {
    let path = temp_db_path("redispatch-storm-deadworker");
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
            name: "dead-worker".to_owned(),
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
    let dead = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build())
        .unwrap();

    let result = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(
        result.status, ExecutionStatus::Ready,
        "a fresh ready execution must be created"
    );
    assert_ne!(result.id, dead.id, "must not reuse the dead execution");
    let dead_after = query_execution(&db.connect().unwrap(), &dead.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        dead_after.status, ExecutionStatus::Abandoned,
        "the un-claimed live-status row must be abandoned",
    );
    let _ = std::fs::remove_file(path);
}

/// Detachment fix (`task_18b347260cd7da80_e`). `current_execution_id`
/// (`TaskRuntime.execution_id`) and the kanban card must follow the
/// live worker, not a newer terminal phantom left behind by a
/// re-dispatch storm.
#[test]
fn task_runtime_follows_live_execution_not_newer_terminal() {
    let path = temp_db_path("runtime-follows-live");
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
            name: "R693".to_owned(),
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build())
        .unwrap();
    // Newer terminal phantom that detaches the card under the bug.
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Orphaned)
        .build())
    .unwrap();

    let runtime = db.get_task_runtime(&chore.id).unwrap();
    assert_eq!(
        runtime.execution_id.as_deref(),
        Some(live.id.as_str()),
        "runtime must point at the live execution, not the newer phantom",
    );
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Running));
    let _ = std::fs::remove_file(path);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
    assert_eq!(orphaned.status, ExecutionStatus::Orphaned);
    assert!(
        orphaned.finished_at.is_some(),
        "orphan reap must stamp finished_at",
    );
    // Workspace columns MUST be preserved — that's the whole
    // contract that lets the next worker resume the same branch.
    assert_eq!(orphaned.cube_lease_id.as_deref(), Some("lease-ORPH"));
    assert_eq!(
        orphaned.cube_workspace_id.as_deref(),
        Some("mono-agent-004")
    );
    assert_eq!(
        orphaned.workspace_path.as_deref(),
        Some("/tmp/mono-agent-004")
    );

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Completed)
            .build())
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

/// A pane-spawn failure must pull the card out of `active` and back
/// to `todo`, stamping `last_status_actor = 'engine'`. Without it
/// the card is stranded green in Doing and the orphan-active sweep
/// re-dispatches the same doomed spawn every cycle. The guard must
/// only fire on `active` rows (never stomping a `done`/`blocked`
/// move) and report whether it demoted anything.
#[test]
fn demote_active_work_item_to_todo_resets_active_card() {
    let path = temp_db_path("demote-active-to-todo");
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
            name: "Stuck in Doing".to_owned(),
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
    // Human dragged it to Doing → active, stamped 'human'.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.force_last_status_actor_for_test(&chore.id, "human")
        .unwrap();

    let status_and_actor = |db: &WorkDb| match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => (t.status, t.last_status_actor),
        other => panic!("expected chore, got {other:?}"),
    };

    // First demote returns true and resets to todo + engine actor.
    assert!(db.demote_active_work_item_to_todo(&chore.id).unwrap());
    let (status, actor) = status_and_actor(&db);
    assert_eq!(status, TaskStatus::Todo);
    assert_eq!(actor, "engine");

    // Idempotent: a second demote finds no `active` row → false,
    // and does not touch the now-`todo` row.
    assert!(!db.demote_active_work_item_to_todo(&chore.id).unwrap());
    assert_eq!(status_and_actor(&db).0, TaskStatus::Todo);

    // Guard: a `done` card is never stomped back to todo.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!db.demote_active_work_item_to_todo(&chore.id).unwrap());
    assert_eq!(status_and_actor(&db).0, TaskStatus::Done);

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
    db.mark_execution_orphaned(&execution.id, "test orphan")
        .unwrap();

    // Reconcile pass: predecessor is terminal-orphaned, so a fresh
    // ready row is inserted. The new row must inherit the orphan's
    // workspace_id as its preferred_workspace_id.
    let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
    assert_eq!(redispatched, vec![chore.id.clone()]);

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 2);
    let orphan_after = executions.iter().find(|e| e.id == execution.id).unwrap();
    assert_eq!(orphan_after.status, ExecutionStatus::Orphaned);
    // Workspace preserved on the orphan row.
    assert_eq!(
        orphan_after.cube_workspace_id.as_deref(),
        Some("mono-agent-005")
    );

    let fresh = executions.iter().find(|e| e.id != execution.id).unwrap();
    assert_eq!(fresh.status, ExecutionStatus::Ready);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::WaitingHuman)
            .build())
        .unwrap();

    let returned = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

/// Regression: a chore that has been manually moved directly to
/// `in_review` (without going through `active`) must NOT receive a
/// `ready` execution from `reconcile_product_executions`.
///
/// Before the fix, `task_accepts_execution` only blocked `done` and
/// `todo+autostart=false`; an `in_review` chore passed the gate and
/// `reconcile_work_item_execution` created a `ready` row for it, which
/// the dispatcher immediately turned into a worker launch.
#[test]
fn reconcile_skips_in_review_chore() {
    let path = temp_db_path("in-review-no-dispatch");
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
            name: "Already in review".to_owned(),
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

    // Simulate `boss task update --status in-review`:
    // the chore moves directly from `todo` to `in_review` without
    // going through `active` (e.g. the PR was opened manually).
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let updated = db.get_work_item(&chore.id).unwrap();
    let updated_task = match updated {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        _ => panic!("expected chore"),
    };
    assert_eq!(updated_task.status, TaskStatus::InReview);

    // reconcile_product_executions must NOT create an execution.
    let result = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        result.created.is_empty(),
        "in_review chore must not get a ready execution from reconcile, got {:?}",
        result.created,
    );
    assert!(
        db.list_executions(Some(&chore.id)).unwrap().is_empty(),
        "no execution must exist for in_review chore after reconcile",
    );
}

/// Regression: the specific bug sequence that triggered the incident:
/// create `--no-autostart`, bind a PR, move directly to `in_review`,
/// then call `reconcile_product_executions`. No execution must be
/// created.
#[test]
fn no_autostart_direct_to_in_review_suppresses_dispatch() {
    let path = temp_db_path("no-autostart-in-review");
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
    // Step 1: create with --no-autostart (autostart=false)
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "Design doc T708".to_owned(),
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
    assert!(!chore.autostart);

    // Reconcile right after create — no execution (autostart=false + todo).
    let r = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        r.created.is_empty(),
        "step 1: no execution for no-autostart todo chore"
    );

    // Step 2: bind a PR (simulates `boss task bind-pr`). The PR patch
    // goes through UpdateWorkItem; the reconcile that follows must still
    // be a no-op because the task is still in `todo` with autostart=false.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/spinyfin/mono/pull/821".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let r = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        r.created.is_empty(),
        "step 2: still no execution after bind-pr"
    );

    // Step 3: manually move to in_review (simulates
    // `boss task update --status in-review`). This is the exact
    // trigger that fired a spurious dispatch before the fix.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let r = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        r.created.is_empty(),
        "step 3: in_review chore must not get an execution after reconcile, got {:?}",
        r.created,
    );
    assert!(
        db.list_executions(Some(&chore.id)).unwrap().is_empty(),
        "no execution row must exist at all for T708-shaped chore",
    );
}

/// `cancel_running_execution_and_demote_task`: verify that a `running`
/// execution is set to `cancelled` and the owning `active` task is
/// moved back to `todo`. Calling it a second time is a no-op.
#[test]
fn cancel_running_execution_demotes_active_task() {
    let path = temp_db_path("cancel-exec-demote");
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
            name: "Running chore".to_owned(),
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
    // Manually place the chore in `active` with a `running` execution,
    // simulating what `start_execution_run` does.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let exec = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build())
        .unwrap();

    // First call: should cancel execution + demote task.
    let (cancelled, demoted) = db
        .cancel_running_execution_and_demote_task(&exec.id)
        .unwrap();
    assert!(cancelled, "execution must be marked cancelled");
    assert!(demoted, "task must be demoted from active to todo");

    let updated_exec = db.get_execution(&exec.id).unwrap();
    assert_eq!(updated_exec.status, ExecutionStatus::Cancelled);
    let updated_task = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        _ => panic!("expected chore"),
    };
    assert_eq!(updated_task.status, TaskStatus::Todo);

    // Second call: both ops must be no-ops (idempotent).
    let (cancelled2, demoted2) = db
        .cancel_running_execution_and_demote_task(&exec.id)
        .unwrap();
    assert!(!cancelled2, "second call: execution already terminal");
    assert!(!demoted2, "second call: task already out of active");
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Failed)
        .build())
    .unwrap();

    let redispatched = db.rescan_active_dispatch().unwrap();
    assert_eq!(redispatched, vec![chore.id.clone()]);

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 2);
    assert_eq!(executions.last().unwrap().status, ExecutionStatus::Ready);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert_eq!(executions[0].status, ExecutionStatus::Ready);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Ready)
        .build())
    .unwrap();

    let redispatched = db.rescan_active_dispatch().unwrap();
    assert!(
        redispatched.is_empty(),
        "non-terminal execution should be left alone, got {redispatched:?}",
    );
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "no duplicate ready row should be inserted"
    );
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    let tasks = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap();
    let design = tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("project should have an auto-created design task");
    assert_eq!(design.name, "Design Engine dispatch instrumentation");
    assert_eq!(design.status, TaskStatus::Todo);
    assert_eq!(design.ordinal, Some(0));
    assert_eq!(design.project_id.as_deref(), Some(project.id.as_str()));

    // Reconcile lights up the design task as a `project_design`
    // execution — same machinery as chore/task dispatch, just a
    // different kind on the work_executions row.
    db.reconcile_product_executions(&product.id).unwrap();
    let executions = db.list_executions(Some(&design.id)).unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].kind, ExecutionKind::ProjectDesign);
    assert_eq!(executions[0].status, ExecutionStatus::Ready);

    // The matching task runtime is in the work tree — that's
    // what the kanban joins to render the activity dot. No
    // separate "project runtime" needed.
    let tree = db.get_work_tree(&product.id).unwrap();
    let runtime = tree
        .task_runtimes
        .iter()
        .find(|r| r.work_item_id == design.id)
        .expect("design task runtime missing from work tree");
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Ready));

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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

    let tasks = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap();
    let design = tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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

    let tasks = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap();
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
    let tasks = db
        .list_tasks(&project.product_id, Some(&project_id), None, false)
        .unwrap();
    let design = tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("migration should backfill a design task");
    assert_eq!(design.status, TaskStatus::Todo);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    db.create_execution(CreateExecutionInput::builder()
        .work_item_id(chore.id.clone())
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Failed)
        .build())
    .unwrap();

    let redispatched = db.rescan_active_dispatch().unwrap();
    assert!(
        redispatched.is_empty(),
        "autostart=false items must stay parked, got {redispatched:?}",
    );
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "no fresh ready row for autostart=false"
    );
    assert_eq!(executions[0].status, ExecutionStatus::Failed);
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
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
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
    assert!(
        chore.autostart,
        "newly created chore should have autostart=true"
    );

    // Place a ready execution so start_execution_run can run.
    let execution = db
        .create_execution(CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build())
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
            assert_eq!(t.status, TaskStatus::Active);
            assert!(
                !t.autostart,
                "autostart must be cleared after first Doing transition"
            );
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
        conn.query_row("SELECT autostart FROM tasks WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
    };

    assert_eq!(
        autostart_for("task-todo"),
        1,
        "todo row must keep autostart=1"
    );
    assert_eq!(
        autostart_for("task-active"),
        0,
        "active row must be cleared to autostart=0"
    );
    assert_eq!(
        autostart_for("task-done"),
        0,
        "done row must be cleared to autostart=0"
    );
    assert_eq!(
        autostart_for("task-blocked"),
        0,
        "blocked row must be cleared to autostart=0"
    );

    let version: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "19");

    let _ = std::fs::remove_file(path);
}

// ── stale-lease reclaim for UI-crash resume (issue #962) ──────────

/// Helper: create a product + chore and a single ready execution under
/// it, returning the execution id. Mirrors the in-flight setup the
/// other t02 reconcile tests use.
fn make_chore_execution_962(db: &WorkDb, label: &str) -> String {
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name(format!("Prod-{label}"))
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("Chore-{label}"))
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .unwrap()
    .id
}

/// The mono-agent-003 scenario: a worker started a run, the app
/// crashed, and the startup reaper marked the execution `orphaned`
/// while preserving its lease columns. Cube still reports the
/// workspace `leased` to that dead lease. The reclaim helper must
/// surface that lease id so the dispatcher can force-release it before
/// the resume re-leases the same workspace.
#[test]
fn stale_lease_reclaim_returns_lease_of_orphaned_owner() {
    let db = WorkDb::open(temp_db_path("reclaim-orphaned")).unwrap();
    let exec_id = make_chore_execution_962(&db, "reclaim-orphaned");
    db.start_execution_run(&exec_id, "agent-1", "repo-1", "lease-dead", "ws-1", "/ws/ws-1")
        .unwrap();
    db.mark_execution_orphaned(&exec_id, "ui crash").unwrap();

    let reclaim = db
        .stale_lease_to_reclaim_for_workspace("ws-1", "lease-dead")
        .unwrap();
    assert_eq!(
        reclaim.as_deref(),
        Some("lease-dead"),
        "orphaned owner's lease should be reclaimable"
    );
}

/// Safety: never reclaim a lease while a live (`running`) execution
/// still claims the workspace — that lease is genuinely in use.
#[test]
fn stale_lease_reclaim_skips_when_live_execution_claims_workspace() {
    let db = WorkDb::open(temp_db_path("reclaim-live")).unwrap();
    let exec_id = make_chore_execution_962(&db, "reclaim-live");
    db.start_execution_run(&exec_id, "agent-1", "repo-1", "lease-live", "ws-1", "/ws/ws-1")
        .unwrap();
    // Execution is still `running` — its lease must not be reclaimed.
    let reclaim = db
        .stale_lease_to_reclaim_for_workspace("ws-1", "lease-live")
        .unwrap();
    assert_eq!(
        reclaim, None,
        "a live execution's lease must never be reclaimed"
    );
}

/// Safety: never reclaim when cube's current lease id does not match
/// the lease the engine recorded for the terminal execution — the slot
/// has since been taken by an unrelated lease the engine doesn't own.
#[test]
fn stale_lease_reclaim_skips_when_lease_id_does_not_match() {
    let db = WorkDb::open(temp_db_path("reclaim-mismatch")).unwrap();
    let exec_id = make_chore_execution_962(&db, "reclaim-mismatch");
    db.start_execution_run(&exec_id, "agent-1", "repo-1", "lease-old", "ws-1", "/ws/ws-1")
        .unwrap();
    db.mark_execution_orphaned(&exec_id, "ui crash").unwrap();

    // Cube now reports a *different* lease holding the workspace.
    let reclaim = db
        .stale_lease_to_reclaim_for_workspace("ws-1", "lease-new")
        .unwrap();
    assert_eq!(
        reclaim, None,
        "only the dead execution's own recorded lease is eligible for reclaim"
    );
}

/// A workspace the engine has no terminal record for must not be
/// reclaimed (defensive default — never force-release an unknown lease).
#[test]
fn stale_lease_reclaim_skips_unknown_workspace() {
    let db = WorkDb::open(temp_db_path("reclaim-unknown")).unwrap();
    let reclaim = db
        .stale_lease_to_reclaim_for_workspace("ws-nonexistent", "lease-x")
        .unwrap();
    assert_eq!(reclaim, None);
}
