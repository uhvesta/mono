//! Tests for the followup task kind: creation via block_pending_revisions_on_parent_close,
//! provenance fields (origin_task_short_id / origin_pr_number), body-text rewrite,
//! and visibility to the merge-poller candidate lists.

use super::*;

const FOLLOWUP_PR_URL: &str = "https://github.com/spinyfin/mono/pull/1537";

/// A revision whose `created_via` starts with `"pr_review:"` must be
/// converted to a `followup` (not a plain `chore`) when the parent PR merges.
/// The followup must carry origin provenance and the rewritten description.
#[test]
fn pr_review_revision_creates_followup_with_correct_kind_and_provenance() {
    let db = WorkDb::open(temp_db_path("followup-creation")).unwrap();
    let product_id = make_revision_product(&db, "fu-create");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Verify the parent has a short_id so provenance can be recorded.
    let parent_task = db.get_work_item(&parent_id).unwrap();
    let parent_short_id = match &parent_task {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.short_id,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert!(parent_short_id.is_some(), "parent must have a short_id for provenance");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address all findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_123"))
                .build(),
            &checker,
        )
        .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // Revision must be archived.
    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status,
        TaskStatus::Archived,
        "pr_review revision must be archived after parent merges"
    );

    // A followup must be created (not a plain chore).
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup);
    assert!(
        followup.is_some(),
        "a followup task must be created for a pr_review revision; chores: {chores:?}",
    );
    let followup = followup.unwrap();

    // No plain chore must be created (only the followup).
    let plain_chore = chores.iter().find(|c| c.id != parent_id && c.kind == TaskKind::Chore);
    assert!(
        plain_chore.is_none(),
        "no plain chore should exist alongside the followup; chores: {chores:?}",
    );

    // Provenance: origin_task_short_id must match the chain-root (parent chore).
    assert_eq!(
        followup.origin_task_short_id, parent_short_id,
        "followup must carry the parent's short_id as origin_task_short_id",
    );

    // Provenance: origin_pr_number must be extracted from the parent pr_url (1537).
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "followup must carry the PR number from the parent's pr_url",
    );

    // Description: old wording replaced.
    assert!(
        !followup.description.contains("finalising this revision"),
        "followup description must not contain 'finalising this revision'",
    );
    assert!(
        followup.description.contains("closing this follow-up"),
        "followup description must contain 'closing this follow-up'",
    );
}

/// A pr_review revision in the `active` (WIP) state must produce an autostart
/// followup so the work is immediately redispatched on a fresh PR.
#[test]
fn pr_review_active_revision_creates_autostart_followup() {
    let db = WorkDb::open(temp_db_path("followup-active")).unwrap();
    let product_id = make_revision_product(&db, "fu-active");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address all findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_456"))
                .build(),
            &checker,
        )
        .unwrap();

    // Simulate the revision being dispatched.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup);
    assert!(
        followup.is_some(),
        "a followup must be created for a WIP pr_review revision"
    );
    assert!(
        followup.unwrap().autostart,
        "WIP pr_review revision must produce autostart followup"
    );
}

/// A followup in `in_review` with a `pr_url` must appear in
/// `list_chores_pending_merge_check` so the merge poller can flip it to `done`.
#[test]
fn followup_visible_to_merge_check_poller() {
    let db = WorkDb::open(temp_db_path("followup-merge-check")).unwrap();
    let product_id = make_revision_product(&db, "fu-merge");
    let parent_pr = "https://github.com/spinyfin/mono/pull/1000";
    let parent_id = make_in_review_chore(&db, &product_id, parent_pr);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(parent_id.clone())
            .description("Address all findings before finalising this revision.")
            .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_789"))
            .build(),
        &checker,
    )
    .unwrap();

    db.mark_chore_pr_merged(&parent_id, parent_pr).unwrap();

    // Find the newly created followup.
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup)
        .expect("a followup must be created");

    // Before the followup has its own PR, it should NOT appear in the merge-check list.
    let before = db.list_chores_pending_merge_check().unwrap();
    assert!(
        !before.iter().any(|p| p.work_item_id == followup.id),
        "followup without pr_url must not appear in merge-check list",
    );

    // Simulate the followup getting its own PR and moving to in_review.
    let followup_pr = "https://github.com/spinyfin/mono/pull/9999";
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        rusqlite::params![followup.id, followup_pr],
    )
    .unwrap();
    drop(conn);

    // Now it must appear in the merge-check list so the merge poller can close it.
    let after = db.list_chores_pending_merge_check().unwrap();
    let found = after.iter().find(|p| p.work_item_id == followup.id);
    assert!(
        found.is_some(),
        "followup in in_review with pr_url must appear in list_chores_pending_merge_check; \
         found ids: {:?}",
        after.iter().map(|p| &p.work_item_id).collect::<Vec<_>>(),
    );
    assert_eq!(found.unwrap().pr_url, followup_pr);
}

/// list_chores must return followup provenance (origin_task_short_id /
/// origin_pr_number) — not None — so the macOS list path renders the
/// Origin row correctly.
#[test]
fn list_chores_returns_followup_provenance() {
    let db = WorkDb::open(temp_db_path("followup-provenance")).unwrap();
    let product_id = make_revision_product(&db, "fu-prov");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(parent_id.clone())
            .description("Address all findings before finalising this revision.")
            .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_prov_test"))
            .build(),
        &checker,
    )
    .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup)
        .expect("a followup must be created");

    assert!(
        followup.origin_task_short_id.is_some(),
        "list_chores must populate origin_task_short_id for followups; got None",
    );
    assert!(
        followup.origin_pr_number.is_some(),
        "list_chores must populate origin_pr_number for followups; got None",
    );
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "origin_pr_number must be parsed from parent's pr_url",
    );
}
