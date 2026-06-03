use super::*;

// ── find_work_items_by_pr (boss task by-pr) ─────────────────────────────────

/// The original miss: a chore-backed PR must be findable by PR number.
/// `list_tasks` omits `kind = chore` rows entirely, so this is the case
/// the by-pr lookup exists to fix.
#[test]
fn find_by_pr_finds_chore_backed_pr() {
    let db = WorkDb::open(temp_db_path("by-pr-chore")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-chore");
    let pr_url = "https://github.com/spinyfin/mono/pull/959";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let matches = db.find_work_items_by_pr(959).unwrap();
    assert_eq!(matches.len(), 1, "exactly one owner expected");
    assert_eq!(matches[0].owner.id, chore_id);
    assert_eq!(matches[0].owner.kind, TaskKind::Chore);
    assert!(matches[0].revisions.is_empty());
}

/// Number parsing is robust to the same query/fragment suffixes the
/// merge poller tolerates.
#[test]
fn find_by_pr_tolerates_url_suffixes() {
    let db = WorkDb::open(temp_db_path("by-pr-suffix")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-suffix");
    let pr_url = "https://github.com/spinyfin/mono/pull/77?foo=bar#discussion";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let matches = db.find_work_items_by_pr(77).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].owner.id, chore_id);
}

#[test]
fn find_by_pr_returns_empty_when_unbound() {
    let db = WorkDb::open(temp_db_path("by-pr-none")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-none");
    make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/1");

    let matches = db.find_work_items_by_pr(42).unwrap();
    assert!(matches.is_empty());
}

/// Revisions commit to the owner's PR without owning a `pr_url`, so
/// they must surface under the owner — ordered R1, R2, … with the
/// owner's PR projected onto `revision_parent_pr_url`.
#[test]
fn find_by_pr_surfaces_chain_revisions() {
    let db = WorkDb::open(temp_db_path("by-pr-revisions")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-revisions");
    let pr_url = "https://github.com/spinyfin/mono/pull/200";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db.create_revision(revision_input(&root_id), &checker).unwrap();
    let r2 = db.create_revision(revision_input(&r1.id), &checker).unwrap();

    let matches = db.find_work_items_by_pr(200).unwrap();
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    assert_eq!(m.owner.id, root_id);
    assert_eq!(m.revisions.len(), 2, "both chain revisions surfaced");
    let rev_ids: Vec<&str> = m.revisions.iter().map(|r| r.id.as_str()).collect();
    assert!(rev_ids.contains(&r1.id.as_str()));
    assert!(rev_ids.contains(&r2.id.as_str()));
    assert_eq!(m.revisions[0].revision_seq, Some(1));
    assert_eq!(m.revisions[1].revision_seq, Some(2));
    assert_eq!(
        m.revisions[0].revision_parent_pr_url.as_deref(),
        Some(pr_url),
        "revision parent PR is the owner's PR"
    );
    assert!(
        m.revisions.iter().all(|r| r.pr_url.is_none()),
        "revisions do not own a pr_url"
    );
}

#[test]
fn find_by_pr_excludes_soft_deleted_owner() {
    let db = WorkDb::open(temp_db_path("by-pr-deleted")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-deleted");
    let pr_url = "https://github.com/spinyfin/mono/pull/300";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    db.delete_work_item(&chore_id).unwrap();

    let matches = db.find_work_items_by_pr(300).unwrap();
    assert!(matches.is_empty(), "soft-deleted owner must not match");
}

/// The same PR number can exist in more than one repo. The engine
/// returns every owner; the CLI disambiguates by repo.
#[test]
fn find_by_pr_returns_multiple_when_number_shared_across_repos() {
    let db = WorkDb::open(temp_db_path("by-pr-ambiguous")).unwrap();
    let product_a = make_revision_product(&db, "ambig-a");
    let product_b = make_revision_product(&db, "ambig-b");
    make_in_review_chore(&db, &product_a, "https://github.com/spinyfin/mono/pull/500");
    make_in_review_chore(&db, &product_b, "https://github.com/spinyfin/other/pull/500");

    let matches = db.find_work_items_by_pr(500).unwrap();
    assert_eq!(matches.len(), 2, "same number in two repos => two owners");
}

// ── automation CRUD ──────────────────────────────────────────────────────────

fn make_schedule_trigger() -> AutomationTrigger {
    AutomationTrigger::Schedule {
        cron: "0 14 * * 1-5".to_owned(),
        timezone: "America/Los_Angeles".to_owned(),
    }
}

fn make_product(db: &WorkDb) -> Product {
    db.create_product(CreateProductInput {
        name: "Automation Test Co".to_owned(),
        description: None,
        repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
        design_repo: None,
        docs_repo: None,
        worker_branch_prefix: None,
    })
    .unwrap()
}

#[test]
fn create_automation_round_trips() {
    let db = WorkDb::open(temp_db_path("auto-create")).unwrap();
    let product = make_product(&db);

    let input = CreateAutomationInput {
        product_id: product.id.clone(),
        name: "Fix clippy".to_owned(),
        repo_remote_url: None,
        trigger: make_schedule_trigger(),
        standing_instruction: "Fix any new clippy warnings.".to_owned(),
        open_task_limit: 1,
        catch_up_window_secs: None,
        enabled: true,
        created_via: Some("cli".to_owned()),
    };

    let auto = db.create_automation(input).unwrap();

    assert!(auto.id.starts_with("auto_"));
    assert_eq!(auto.product_id, product.id);
    assert_eq!(auto.name, "Fix clippy");
    assert_eq!(auto.open_task_limit, 1);
    assert!(auto.enabled);
    assert_eq!(auto.created_via, "cli");
    assert_eq!(auto.short_id, Some(1));
    assert!(auto.last_fired_at.is_none());
    assert!(auto.next_due_at.is_none());

    match &auto.trigger {
        AutomationTrigger::Schedule { cron, timezone } => {
            assert_eq!(cron, "0 14 * * 1-5");
            assert_eq!(timezone, "America/Los_Angeles");
        }
    }
}

#[test]
fn list_automations_returns_empty_for_new_product() {
    let db = WorkDb::open(temp_db_path("auto-list-empty")).unwrap();
    let product = make_product(&db);

    let list = db.list_automations(&product.id).unwrap();
    assert!(list.is_empty());
}

#[test]
fn list_automations_returns_all_for_product() {
    let db = WorkDb::open(temp_db_path("auto-list")).unwrap();
    let product = make_product(&db);

    let a1 = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "A one".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst1".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let _a2 = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "A two".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst2".to_owned(),
            open_task_limit: 2,
            catch_up_window_secs: Some(900),
            enabled: false,
            created_via: None,
        })
        .unwrap();

    let list = db.list_automations(&product.id).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, a1.id);
    assert_eq!(list[0].short_id, Some(1));
    assert_eq!(list[1].short_id, Some(2));
    assert_eq!(list[1].open_task_limit, 2);
    assert!(!list[1].enabled);
    assert_eq!(list[1].catch_up_window_secs, Some(900));
}

#[test]
fn get_automation_returns_none_for_unknown_id() {
    let db = WorkDb::open(temp_db_path("auto-get-none")).unwrap();
    let result = db.get_automation("auto_unknown_000").unwrap();
    assert!(result.is_none());
}

#[test]
fn get_automation_returns_row_by_id() {
    let db = WorkDb::open(temp_db_path("auto-get")).unwrap();
    let product = make_product(&db);

    let created = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Bump deps".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "Bump clean deps.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let fetched = db.get_automation(&created.id).unwrap().unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "Bump deps");
}

#[test]
fn update_automation_applies_patch() {
    let db = WorkDb::open(temp_db_path("auto-update")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Original name".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "original".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let updated = db
        .update_automation(
            &auto.id,
            AutomationPatch {
                name: Some("New name".to_owned()),
                open_task_limit: Some(3),
                enabled: Some(false),
                ..AutomationPatch::default()
            },
        )
        .unwrap();

    assert_eq!(updated.name, "New name");
    assert_eq!(updated.open_task_limit, 3);
    assert!(!updated.enabled);
    assert_eq!(updated.standing_instruction, "original");
}

#[test]
fn enable_disable_automation() {
    let db = WorkDb::open(temp_db_path("auto-enable-disable")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Toggle me".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let disabled = db.disable_automation(&auto.id).unwrap();
    assert!(!disabled.enabled);

    let enabled = db.enable_automation(&auto.id).unwrap();
    assert!(enabled.enabled);
}

#[test]
fn delete_automation_removes_row() {
    let db = WorkDb::open(temp_db_path("auto-delete")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "To be deleted".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "delete me".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    db.delete_automation(&auto.id).unwrap();

    let fetched = db.get_automation(&auto.id).unwrap();
    assert!(fetched.is_none());

    let list = db.list_automations(&product.id).unwrap();
    assert!(list.is_empty());
}

#[test]
fn count_open_tasks_for_automation_zero_when_none() {
    let db = WorkDb::open(temp_db_path("auto-count-zero")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Counter".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "count tasks".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let count = db.count_open_tasks_for_automation(&auto.id).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn count_open_tasks_counts_only_open_statuses() {
    let db = WorkDb::open(temp_db_path("auto-count-open")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Count test".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 5,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    // Create a task and stamp source_automation_id directly (bypassing the
    // not-yet-built create_task --automation flow for this unit test).
    let task = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "chore from automation".to_owned(),
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

    // Stamp the source_automation_id.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        rusqlite::params![auto.id, task.id],
    )
    .unwrap();
    drop(conn);

    // Task is in 'todo' → counts as open.
    assert_eq!(db.count_open_tasks_for_automation(&auto.id).unwrap(), 1);

    // Move task to 'done' → no longer open.
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert_eq!(db.count_open_tasks_for_automation(&auto.id).unwrap(), 0);
}

/// Regression: the kanban label "doing" maps to the DB value `active`.
/// Tasks with status `active` (executing) must be counted as open so the
/// display and the cap gate both reflect work that is in flight.
#[test]
fn count_open_tasks_counts_active_as_open() {
    let db = WorkDb::open(temp_db_path("auto-count-active")).unwrap();
    let product = make_product(&db);

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Active count test".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 5,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let task = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "active chore from automation".to_owned(),
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
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        rusqlite::params![auto.id, task.id],
    )
    .unwrap();
    drop(conn);

    // Move to 'active' (the DB value for the kanban "doing" state).
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Must count as open — not 0 — so an in-flight task blocks the cap.
    assert_eq!(
        db.count_open_tasks_for_automation(&auto.id).unwrap(),
        1,
        "task with status='active' (doing) must be counted as open"
    );
}

#[test]
fn short_ids_are_allocated_per_product() {
    let db = WorkDb::open(temp_db_path("auto-short-ids")).unwrap();
    let p1 = make_product(&db);
    let p2 = db
        .create_product(CreateProductInput {
            name: "Second Product".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();

    let make = |db: &WorkDb, product_id: &str, name: &str| {
        db.create_automation(CreateAutomationInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap()
    };

    let a1p1 = make(&db, &p1.id, "P1 A1");
    let a2p1 = make(&db, &p1.id, "P1 A2");
    let a1p2 = make(&db, &p2.id, "P2 A1");

    assert_eq!(a1p1.short_id, Some(1));
    assert_eq!(a2p1.short_id, Some(2));
    assert_eq!(a1p2.short_id, Some(1));
}

// ── triage execution + outcome detection (Maint task 6) ──────────────────────

fn make_automation(db: &WorkDb, product_id: &str, limit: i64) -> boss_protocol::Automation {
    db.create_automation(CreateAutomationInput {
        product_id: product_id.to_owned(),
        name: "clippy sweep".to_owned(),
        repo_remote_url: None,
        trigger: make_schedule_trigger(),
        standing_instruction: "Fix any clippy warnings.".to_owned(),
        open_task_limit: limit,
        catch_up_window_secs: None,
        enabled: true,
        created_via: Some("cli".to_owned()),
    })
    .unwrap()
}

/// `create_automation_task` stamps provenance, defaults the produced row to a
/// product-level autostart chore, and — the fan-out backstop — refuses a
/// second create once the open-task cap is reached.
#[test]
fn create_automation_task_stamps_provenance_and_enforces_cap() {
    let db = WorkDb::open(temp_db_path("auto-task-cap")).unwrap();
    let product = make_product(&db);
    let automation = make_automation(&db, &product.id, 1);

    let task = db
        .create_automation_task(&automation.id, "fix clippy in foo", Some("the foo crate"))
        .unwrap();
    assert_eq!(task.kind, TaskKind::Chore);
    assert_eq!(task.project_id, None);
    assert!(task.autostart);
    assert_eq!(task.source_automation_id.as_deref(), Some(automation.id.as_str()));
    assert_eq!(
        db.count_open_tasks_for_automation(&automation.id).unwrap(),
        1
    );

    // Second create must be rejected by the transactional cap re-check.
    let err = db
        .create_automation_task(&automation.id, "fix clippy in bar", None)
        .unwrap_err();
    assert!(
        err.to_string().contains("open-task limit"),
        "expected cap error, got: {err}"
    );
    assert_eq!(
        db.count_open_tasks_for_automation(&automation.id).unwrap(),
        1,
        "rejected create must not insert a row"
    );

    // The produced task is listed under the automation.
    let tasks = db.list_tasks_for_automation(&automation.id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task.id);
}

/// A higher cap permits more concurrent produced tasks.
#[test]
fn create_automation_task_respects_higher_cap() {
    let db = WorkDb::open(temp_db_path("auto-task-cap-2")).unwrap();
    let product = make_product(&db);
    let automation = make_automation(&db, &product.id, 2);

    db.create_automation_task(&automation.id, "t1", None).unwrap();
    db.create_automation_task(&automation.id, "t2", None).unwrap();
    assert!(
        db.create_automation_task(&automation.id, "t3", None).is_err(),
        "third create must trip the cap of 2"
    );
}

/// A triage execution binds to the automation (not a task) and starts `ready`
/// in the `automation_triage` kind with the supplied repo.
#[test]
fn create_automation_triage_execution_binds_to_automation() {
    let db = WorkDb::open(temp_db_path("auto-triage-exec")).unwrap();
    let product = make_product(&db);
    let automation = make_automation(&db, &product.id, 1);

    let exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();
    assert_eq!(exec.work_item_id, automation.id);
    assert_eq!(exec.kind, ExecutionKind::AutomationTriage);
    assert_eq!(exec.status, "ready");
    assert_eq!(exec.repo_remote_url, "git@github.com:spinyfin/mono.git");
}

/// The outcome detector finalises the run keyed on the triage execution id,
/// records the produced task, mirrors `last_outcome`, and — crucially — does
/// NOT rewind `next_due_at` (the scheduler already advanced it at fire time).
#[test]
fn finalize_automation_triage_run_records_outcome_without_rewinding_schedule() {
    let db = WorkDb::open(temp_db_path("auto-triage-finalize")).unwrap();
    let product = make_product(&db);
    let automation = make_automation(&db, &product.id, 1);
    let exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();

    // Scheduler-style fire record: pessimistic failed_will_retry, advance to
    // the following occurrence.
    let scheduled_for = 1_700_000_000i64;
    let following = scheduled_for + 86_400;
    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .triage_execution_id(exec.id.clone())
            .next_due_at(following)
            .build(),
    )
    .unwrap();

    // The triage agent created the produced task (real row — `produced_task_id`
    // is a FK into `tasks`, so a verified id is required).
    let produced = db
        .create_automation_task(&automation.id, "fix clippy", None)
        .unwrap();

    // Detector flips the run to produced_task once the worker emitted the marker.
    let updated = db
        .finalize_automation_triage_run(
            &exec.id,
            boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK,
            Some(&produced.id),
            None,
        )
        .unwrap();
    assert!(updated, "a matching run row must be finalised");

    let run = db
        .automation_run_for_triage_execution(&exec.id)
        .unwrap()
        .expect("run row present");
    assert_eq!(run.outcome, boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK);
    assert_eq!(run.produced_task_id.as_deref(), Some(produced.id.as_str()));
    assert!(run.finished_at.is_some());

    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some(boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK)
    );
    assert_eq!(
        reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
        following,
        "finalisation must not rewind the schedule"
    );

    // Finalising an unknown execution id is a no-op, not an error.
    assert!(!db
        .finalize_automation_triage_run(
            "exec_nonexistent",
            boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
            None,
            Some("x"),
        )
        .unwrap());
}
