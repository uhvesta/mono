//! Attention store tests (design: `tools/boss/docs/designs/attentions.md`).
//! Covers grouping-key reconciliation, the generation bump, the member
//! answer_state machine + partial-answer group state, and dismissal.

use super::*;

/// Build a product with one project and one task; return their ids.
fn fixture() -> (WorkDb, String, String, String) {
    let db = WorkDb::open(temp_db_path("attentions")).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Attentions")
                .goal("goal")
                .build(),
        )
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Engine core")
                .build(),
        )
        .unwrap();
    (db, product.id, project.id, task.id)
}

fn question(project_id: &str, doc_path: &str, prompt: &str) -> CreateAttentionInput {
    CreateAttentionInput::builder()
        .kind("question")
        .association_project_id(project_id)
        .source_kind("design_doc")
        .source_doc_path(doc_path)
        .question_type("prompt")
        .prompt_text(prompt)
        .build()
}

fn followup(task_id: &str, name: &str) -> CreateAttentionInput {
    CreateAttentionInput::builder()
        .kind("followup")
        .association_task_id(task_id)
        .source_kind("task_transcript")
        .source_task_id(task_id)
        .proposed_name(name)
        .proposed_description("do the thing")
        .build()
}

#[test]
fn create_question_assigns_group_short_id_and_grouping_key() {
    let (db, product_id, project_id, _task) = fixture();
    let (attention, group) = db
        .create_attention(question(&project_id, "docs/x.md", "one table or two?"))
        .unwrap();

    assert_eq!(group.kind, "question");
    assert_eq!(group.state, "open");
    assert_eq!(group.product_id, product_id);
    assert_eq!(group.generation, 0);
    assert_eq!(group.short_id, Some(1));
    assert_eq!(
        group.grouping_key,
        format!("question|{project_id}|doc:docs/x.md")
    );
    assert_eq!(group.association_project_id.as_deref(), Some(project_id.as_str()));
    assert!(group.association_task_id.is_none());

    assert_eq!(attention.group_id, group.id);
    assert_eq!(attention.ordinal, 1);
    assert_eq!(attention.answer_state, "open");
    assert_eq!(attention.question_type.as_deref(), Some("prompt"));
    assert_eq!(attention.confidence_source, "structured");
}

#[test]
fn same_grouping_key_reconciles_into_one_group() {
    let (db, _product, project_id, _task) = fixture();
    let (_a1, g1) = db
        .create_attention(question(&project_id, "docs/x.md", "Q1"))
        .unwrap();
    let (a2, g2) = db
        .create_attention(question(&project_id, "docs/x.md", "Q2"))
        .unwrap();

    assert_eq!(g1.id, g2.id, "same key must reconcile into one group");
    assert_eq!(a2.ordinal, 2);
    let members = db.list_attentions_for_group(&g1.id).unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].ordinal, 1);
    assert_eq!(members[1].ordinal, 2);
}

#[test]
fn different_doc_paths_make_separate_groups() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g1) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let (_b, g2) = db
        .create_attention(question(&project_id, "docs/y.md", "Q"))
        .unwrap();
    assert_ne!(g1.id, g2.id);
    assert_eq!(g1.short_id, Some(1));
    assert_eq!(g2.short_id, Some(2), "short ids are dense per product");
}

#[test]
fn followup_grouping_key_is_task_scoped() {
    let (db, _product, _project, task_id) = fixture();
    let (_a, g) = db.create_attention(followup(&task_id, "extract helper")).unwrap();
    assert_eq!(g.kind, "followup");
    assert_eq!(g.grouping_key, format!("followup|{task_id}"));
    assert_eq!(g.association_task_id.as_deref(), Some(task_id.as_str()));
    assert!(g.association_project_id.is_none());
}

#[test]
fn answering_one_member_moves_group_to_partially_answered() {
    let (db, _product, project_id, _task) = fixture();
    let (a1, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q1"))
        .unwrap();
    let (_a2, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q2"))
        .unwrap();

    let group = db
        .answer_attention(&a1.id, Some("because".to_owned()), false, false)
        .unwrap();
    assert_eq!(group.state, "partially_answered");

    let members = db.list_attentions_for_group(&group.id).unwrap();
    let answered = members.iter().find(|m| m.id == a1.id).unwrap();
    assert_eq!(answered.answer_state, "answered");
    assert_eq!(answered.answer.as_deref(), Some("because"));
    assert!(answered.answered_at.is_some());
    let other = members.iter().find(|m| m.id != a1.id).unwrap();
    assert_eq!(other.answer_state, "open");
}

#[test]
fn answering_a_question_without_a_value_is_rejected() {
    let (db, _product, project_id, _task) = fixture();
    let (a, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let err = db.answer_attention(&a.id, None, false, false).unwrap_err();
    assert!(
        err.to_string().contains("requires an answer value"),
        "unexpected error: {err}"
    );
}

#[test]
fn accepting_a_followup_needs_no_answer_value() {
    let (db, _product, _project, task_id) = fixture();
    let (a, _) = db.create_attention(followup(&task_id, "do it")).unwrap();
    let group = db.answer_attention(&a.id, None, false, false).unwrap();
    assert_eq!(group.state, "partially_answered");
    let members = db.list_attentions_for_group(&group.id).unwrap();
    assert_eq!(members[0].answer_state, "answered");
}

#[test]
fn skip_marks_member_skipped_and_clears_answer() {
    let (db, _product, project_id, _task) = fixture();
    let (a, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    // Answer, then skip — skipping must clear the captured answer.
    db.answer_attention(&a.id, Some("maybe".to_owned()), false, false)
        .unwrap();
    let group = db.answer_attention(&a.id, None, true, false).unwrap();
    let member = &db.list_attentions_for_group(&group.id).unwrap()[0];
    assert_eq!(member.answer_state, "skipped");
    assert!(member.answer.is_none());
    assert!(member.answered_at.is_none());
}

#[test]
fn dismiss_member_via_dismiss_attention() {
    let (db, _product, project_id, _task) = fixture();
    let (a, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let group = db.dismiss_attention(&a.id, Some("noise".to_owned())).unwrap();
    // Group is not terminal — only the member was dismissed.
    assert_eq!(group.state, "partially_answered");
    let member = &db.list_attentions_for_group(&group.id).unwrap()[0];
    assert_eq!(member.answer_state, "dismissed");
}

#[test]
fn dismiss_group_is_terminal_and_idempotent() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let dismissed = db.dismiss_attention(&g.id, None).unwrap();
    assert_eq!(dismissed.state, "dismissed");
    assert!(dismissed.dismissed_at.is_some());
    // Idempotent second dismiss.
    let again = db.dismiss_attention(&g.id, None).unwrap();
    assert_eq!(again.state, "dismissed");
}

#[test]
fn reconcile_bumps_generation_past_a_dismissed_group() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g0) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    db.dismiss_attention(&g0.id, None).unwrap();

    // Same key again — must NOT reopen the dismissed group; a new generation.
    let (_b, g1) = db
        .create_attention(question(&project_id, "docs/x.md", "Q again"))
        .unwrap();
    assert_ne!(g1.id, g0.id);
    assert_eq!(g1.generation, 1);
    assert_eq!(g1.grouping_key, g0.grouping_key);
    assert_eq!(g1.state, "open");
}

#[test]
fn members_cannot_join_a_terminal_group_by_explicit_id() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    db.dismiss_attention(&g.id, None).unwrap();

    let mut input = question(&project_id, "docs/x.md", "late");
    input.group_id = Some(g.id.clone());
    let err = db.create_attention(input).unwrap_err();
    assert!(err.to_string().contains("terminal"), "unexpected error: {err}");
}

#[test]
fn answering_a_member_of_a_dismissed_group_is_rejected() {
    let (db, _product, project_id, _task) = fixture();
    let (a, g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    db.dismiss_attention(&g.id, None).unwrap();
    let err = db
        .answer_attention(&a.id, Some("x".to_owned()), false, false)
        .unwrap_err();
    assert!(err.to_string().contains("terminal"), "unexpected error: {err}");
}

#[test]
fn adding_a_member_after_a_partial_answer_keeps_partially_answered() {
    let (db, _product, project_id, _task) = fixture();
    let (a1, _) = db
        .create_attention(question(&project_id, "docs/x.md", "Q1"))
        .unwrap();
    db.answer_attention(&a1.id, Some("yes".to_owned()), false, false)
        .unwrap();
    // A fresh open member must not regress the group back to `open`.
    let (_a2, group) = db
        .create_attention(question(&project_id, "docs/x.md", "Q2"))
        .unwrap();
    assert_eq!(group.state, "partially_answered");
}

#[test]
fn list_applies_kind_and_state_filters() {
    let (db, product_id, project_id, task_id) = fixture();
    let (_q, qg) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let (_f, _fg) = db.create_attention(followup(&task_id, "do it")).unwrap();

    // Default (no state): both open groups are actionable.
    let all = db
        .list_attention_groups(&product_id, None, None, None, None)
        .unwrap();
    assert_eq!(all.len(), 2);

    // Kind filter.
    let questions = db
        .list_attention_groups(&product_id, None, None, Some("question"), None)
        .unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].kind, "question");

    // Association filter.
    let by_task = db
        .list_attention_groups(&product_id, None, Some(&task_id), None, None)
        .unwrap();
    assert_eq!(by_task.len(), 1);
    assert_eq!(by_task[0].kind, "followup");

    // Dismiss the question group: default list drops it, explicit state finds it.
    db.dismiss_attention(&qg.id, None).unwrap();
    let default_now = db
        .list_attention_groups(&product_id, None, None, None, None)
        .unwrap();
    assert_eq!(default_now.len(), 1, "dismissed groups drop from the default list");
    let dismissed = db
        .list_attention_groups(&product_id, None, None, None, Some("dismissed"))
        .unwrap();
    assert_eq!(dismissed.len(), 1);
    assert_eq!(dismissed[0].id, qg.id);
}

#[test]
fn get_attention_group_resolves_short_id() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let by_short = db.get_attention_group("A1").unwrap();
    assert_eq!(by_short.id, g.id);
    let by_id = db.get_attention_group(&g.id).unwrap();
    assert_eq!(by_id.id, g.id);
}

#[test]
fn explicit_group_key_override_groups_unrelated_attentions() {
    let (db, _product, project_id, _task) = fixture();
    let mut first = question(&project_id, "docs/a.md", "Q1");
    first.group_key = Some("custom|key".to_owned());
    let (_a, g1) = db.create_attention(first).unwrap();

    // A different doc path would normally make a new group, but the explicit
    // key wins and reconciles into the same group.
    let mut second = question(&project_id, "docs/b.md", "Q2");
    second.group_key = Some("custom|key".to_owned());
    let (_b, g2) = db.create_attention(second).unwrap();
    assert_eq!(g1.id, g2.id);
}

#[test]
fn multiple_choice_question_requires_choice_options() {
    let (db, _product, project_id, _task) = fixture();
    let mut input = question(&project_id, "docs/x.md", "pick one");
    input.question_type = Some("multiple_choice".to_owned());
    let err = db.create_attention(input).unwrap_err();
    assert!(
        err.to_string().contains("choice_options"),
        "unexpected error: {err}"
    );
}

#[test]
fn rejecting_mismatched_kind_on_explicit_group() {
    let (db, _product, project_id, task_id) = fixture();
    let (_q, qg) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    // Try to add a followup to a question group.
    let mut input = followup(&task_id, "nope");
    input.group_id = Some(qg.id.clone());
    let err = db.create_attention(input).unwrap_err();
    assert!(
        err.to_string().contains("does not match group"),
        "unexpected error: {err}"
    );
}

// ── ActionAttentionGroup (task 3) ─────────────────────────────────────────

/// A question whose group records `source_task_id` (the originating design
/// task), so the action path can target that task's PR.
fn question_from_task(
    project_id: &str,
    source_task_id: &str,
    doc_path: &str,
    prompt: &str,
) -> CreateAttentionInput {
    CreateAttentionInput::builder()
        .kind("question")
        .association_project_id(project_id)
        .source_kind("design_doc")
        .source_task_id(source_task_id)
        .source_doc_path(doc_path)
        .question_type("prompt")
        .prompt_text(prompt)
        .build()
}

/// Stamp a task's PR url + status to simulate "doc in review".
fn set_task_pr(db: &WorkDb, task_id: &str, pr_url: &str, status: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = ?2, pr_url = ?3 WHERE id = ?1",
        rusqlite::params![task_id, status, pr_url],
    )
    .unwrap();
}

fn read_task(db: &WorkDb, id: &str) -> Task {
    let conn = db.connect().unwrap();
    query_task(&conn, id).unwrap().unwrap()
}

#[test]
fn action_question_with_open_pr_creates_revision() {
    let (db, _product, project_id, task_id) = fixture();
    set_task_pr(
        &db,
        &task_id,
        "https://github.com/spinyfin/mono/pull/42",
        "in_review",
    );
    let (a, _g) = db
        .create_attention(question_from_task(
            &project_id,
            &task_id,
            "docs/x.md",
            "one table or two?",
        ))
        .unwrap();
    let g = db
        .answer_attention(&a.id, Some("two tables".to_owned()), false, false)
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let actioned = db.action_attention_group(&g.id, false, &checker).unwrap();

    assert_eq!(actioned.group.state, "actioned");
    assert_eq!(actioned.group.produced_artifact_kind.as_deref(), Some("revision"));
    assert!(actioned.group.actioned_at.is_some());
    assert_eq!(actioned.produced_work_item_ids.len(), 1);

    let revision = read_task(&db, &actioned.produced_work_item_ids[0]);
    assert_eq!(revision.kind, "revision");
    assert_eq!(revision.parent_task_id.as_deref(), Some(task_id.as_str()));
    assert_eq!(revision.created_via, "attention");
    assert!(
        revision.description.contains("two tables"),
        "revision should carry the Q&A brief: {}",
        revision.description
    );
    // The produced ref links back to the revision.
    assert!(
        actioned
            .group
            .produced_artifact_ref
            .as_deref()
            .unwrap()
            .contains(&revision.id),
        "ref should reference the revision id"
    );
}

#[test]
fn action_question_with_merged_doc_creates_design_task() {
    let (db, _product, project_id, task_id) = fixture();
    set_task_pr(
        &db,
        &task_id,
        "https://github.com/spinyfin/mono/pull/7",
        "in_review",
    );
    let (a, _g) = db
        .create_attention(question_from_task(
            &project_id,
            &task_id,
            "docs/x.md",
            "gate behind a flag?",
        ))
        .unwrap();
    let g = db
        .answer_attention(&a.id, Some("yes, flag it".to_owned()), false, false)
        .unwrap();

    // The live PR probe reports Merged, so the revision gate refuses and the
    // action falls back to a fresh design task.
    let checker = FakePrStateChecker::always(PrOpenState::Merged);
    let actioned = db.action_attention_group(&g.id, false, &checker).unwrap();

    assert_eq!(actioned.group.state, "actioned");
    assert_eq!(
        actioned.group.produced_artifact_kind.as_deref(),
        Some("design_task")
    );
    assert_eq!(actioned.produced_work_item_ids.len(), 1);

    let design = read_task(&db, &actioned.produced_work_item_ids[0]);
    assert_eq!(design.kind, "design");
    assert_eq!(design.project_id.as_deref(), Some(project_id.as_str()));
    assert_eq!(design.created_via, "attention");
    assert!(
        design.description.contains("yes, flag it") && design.description.contains("docs/x.md"),
        "design task should carry the Q&A brief: {}",
        design.description
    );
}

#[test]
fn action_question_without_source_task_creates_design_task() {
    let (db, _product, project_id, _task) = fixture();
    // The default fixture question carries no source_task_id.
    let (a, _g) = db
        .create_attention(question(&project_id, "docs/y.md", "rename the column?"))
        .unwrap();
    let g = db
        .answer_attention(&a.id, Some("no".to_owned()), false, false)
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let actioned = db.action_attention_group(&g.id, false, &checker).unwrap();
    assert_eq!(
        actioned.group.produced_artifact_kind.as_deref(),
        Some("design_task")
    );
    let design = read_task(&db, &actioned.produced_work_item_ids[0]);
    assert_eq!(design.kind, "design");
}

#[test]
fn action_followup_group_creates_tasks() {
    let (db, _product, project_id, task_id) = fixture();
    let (a1, _g) = db.create_attention(followup(&task_id, "extract helper")).unwrap();
    let (a2, _g) = db.create_attention(followup(&task_id, "add a test")).unwrap();
    // Accept the first, skip the second — only accepted members produce work.
    db.answer_attention(&a1.id, None, false, false).unwrap();
    let g = db.answer_attention(&a2.id, None, true, false).unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let actioned = db.action_attention_group(&g.id, false, &checker).unwrap();

    assert_eq!(actioned.group.state, "actioned");
    assert_eq!(actioned.group.produced_artifact_kind.as_deref(), Some("tasks"));
    assert_eq!(
        actioned.produced_work_item_ids.len(),
        1,
        "only the accepted followup becomes a task"
    );
    let task = read_task(&db, &actioned.produced_work_item_ids[0]);
    assert_eq!(task.kind, "project_task");
    assert_eq!(task.name, "extract helper");
    assert_eq!(task.project_id.as_deref(), Some(project_id.as_str()));
    assert_eq!(task.created_via, "attention");
}

#[test]
fn action_followup_honours_chore_work_kind() {
    let (db, _product, _project, task_id) = fixture();
    let mut input = followup(&task_id, "tidy the logs");
    input.proposed_work_kind = Some("chore".to_owned());
    let (a, _g) = db.create_attention(input).unwrap();
    let g = db.answer_attention(&a.id, None, false, false).unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let actioned = db.action_attention_group(&g.id, false, &checker).unwrap();
    let chore = read_task(&db, &actioned.produced_work_item_ids[0]);
    assert_eq!(chore.kind, "chore");
    assert!(chore.project_id.is_none(), "chores are product-level");
}

#[test]
fn action_requires_every_member_terminal() {
    let (db, _product, project_id, _task) = fixture();
    let (a1, _g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q1"))
        .unwrap();
    let (_a2, _g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q2"))
        .unwrap();
    let g = db
        .answer_attention(&a1.id, Some("yes".to_owned()), false, false)
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let err = db
        .action_attention_group(&g.id, false, &checker)
        .unwrap_err();
    assert!(
        err.to_string().contains("unanswered"),
        "unexpected error: {err}"
    );
    // The group is untouched — still actionable later.
    let reloaded = db.get_attention_group(&g.id).unwrap();
    assert_eq!(reloaded.state, "partially_answered");
}

#[test]
fn action_with_skip_unanswered_clears_open_members_then_actions() {
    let (db, _product, project_id, task_id) = fixture();
    set_task_pr(
        &db,
        &task_id,
        "https://github.com/spinyfin/mono/pull/9",
        "in_review",
    );
    let (a1, _g) = db
        .create_attention(question_from_task(&project_id, &task_id, "docs/x.md", "Q1"))
        .unwrap();
    let (a2, _g) = db
        .create_attention(question_from_task(&project_id, &task_id, "docs/x.md", "Q2"))
        .unwrap();
    db.answer_attention(&a1.id, Some("answered".to_owned()), false, false)
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let actioned = db.action_attention_group(&a2.group_id, true, &checker).unwrap();
    assert_eq!(actioned.group.state, "actioned");
    assert_eq!(actioned.group.produced_artifact_kind.as_deref(), Some("revision"));

    let members = db.list_attentions_for_group(&actioned.group.id).unwrap();
    let skipped = members.iter().find(|m| m.id == a2.id).unwrap();
    assert_eq!(skipped.answer_state, "skipped");
}

#[test]
fn action_all_skipped_question_group_has_nothing_to_produce() {
    let (db, _product, project_id, _task) = fixture();
    let (a, _g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let g = db.answer_attention(&a.id, None, true, false).unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let err = db
        .action_attention_group(&g.id, false, &checker)
        .unwrap_err();
    assert!(
        err.to_string().contains("no answered questions"),
        "unexpected error: {err}"
    );
}

#[test]
fn action_rejects_terminal_group() {
    let (db, _product, project_id, _task) = fixture();
    let (_a, g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    db.dismiss_attention(&g.id, None).unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let err = db
        .action_attention_group(&g.id, true, &checker)
        .unwrap_err();
    assert!(
        err.to_string().contains("dismissed"),
        "unexpected error: {err}"
    );
}

#[test]
fn actioned_group_cannot_be_actioned_again() {
    let (db, _product, project_id, _task) = fixture();
    let (a, _g) = db
        .create_attention(question(&project_id, "docs/x.md", "Q"))
        .unwrap();
    let g = db.answer_attention(&a.id, Some("x".to_owned()), false, false).unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.action_attention_group(&g.id, false, &checker).unwrap();
    let err = db
        .action_attention_group(&g.id, false, &checker)
        .unwrap_err();
    assert!(
        err.to_string().contains("already actioned"),
        "unexpected error: {err}"
    );
}

// --- reconcile_attentions (Attn 4 creation-pipeline upsert) --------------

#[test]
fn reconcile_empty_batch_is_noop() {
    let (db, _product, _project_id, _task) = fixture();
    assert!(db.reconcile_attentions(vec![]).unwrap().is_none());
}

#[test]
fn reconcile_inserts_then_dedupes_identical_batch() {
    let (db, _product, project_id, _task) = fixture();
    let batch = || {
        vec![
            question(&project_id, "docs/x.md", "one table or two?"),
            question(&project_id, "docs/x.md", "gate behind a flag?"),
        ]
    };

    // First reconcile inserts both members into a fresh group.
    let (group, created) = db.reconcile_attentions(batch()).unwrap().unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(group.generation, 0);
    assert_eq!(db.list_attentions_for_group(&group.id).unwrap().len(), 2);

    // Re-running the identical manifest adds nothing and stays in-place.
    let (group2, created2) = db.reconcile_attentions(batch()).unwrap().unwrap();
    assert_eq!(group2.id, group.id);
    assert_eq!(group2.generation, 0);
    assert!(created2.is_empty());
    assert_eq!(db.list_attentions_for_group(&group.id).unwrap().len(), 2);
}

#[test]
fn reconcile_appends_only_new_members() {
    let (db, _product, project_id, _task) = fixture();
    let (group, created) = db
        .reconcile_attentions(vec![question(&project_id, "docs/x.md", "Q1")])
        .unwrap()
        .unwrap();
    assert_eq!(created.len(), 1);

    // A superset batch: Q1 dedupes, only Q2 is appended.
    let (group2, created2) = db
        .reconcile_attentions(vec![
            question(&project_id, "docs/x.md", "Q1"),
            question(&project_id, "docs/x.md", "Q2"),
        ])
        .unwrap()
        .unwrap();
    assert_eq!(group2.id, group.id);
    assert_eq!(created2.len(), 1);
    assert_eq!(created2[0].prompt_text.as_deref(), Some("Q2"));
    let members = db.list_attentions_for_group(&group.id).unwrap();
    assert_eq!(members.len(), 2);
    // Ordinals stay monotonic across reconcile calls.
    assert_eq!(members[0].ordinal, 1);
    assert_eq!(members[1].ordinal, 2);
}

#[test]
fn reconcile_dedupes_within_a_single_batch() {
    let (db, _product, project_id, _task) = fixture();
    let (group, created) = db
        .reconcile_attentions(vec![
            question(&project_id, "docs/x.md", "dup"),
            question(&project_id, "docs/x.md", "dup"),
        ])
        .unwrap()
        .unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(db.list_attentions_for_group(&group.id).unwrap().len(), 1);
}

#[test]
fn reconcile_same_prompt_different_anchor_are_distinct() {
    let (db, _product, project_id, _task) = fixture();
    let mut a = question(&project_id, "docs/x.md", "same?");
    a.source_anchor = Some("schema".to_owned());
    let mut b = question(&project_id, "docs/x.md", "same?");
    b.source_anchor = Some("api".to_owned());
    let (group, created) = db.reconcile_attentions(vec![a, b]).unwrap().unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(db.list_attentions_for_group(&group.id).unwrap().len(), 2);
}

#[test]
fn reconcile_after_terminal_starts_a_new_generation() {
    let (db, _product, project_id, _task) = fixture();
    let (group, _created) = db
        .reconcile_attentions(vec![question(&project_id, "docs/x.md", "Q1")])
        .unwrap()
        .unwrap();
    assert_eq!(group.generation, 0);

    // Close the group; a re-emission must not reopen it.
    let dismissed = db.dismiss_attention(&group.id, None).unwrap();
    assert_eq!(dismissed.state, "dismissed");

    let (group2, created2) = db
        .reconcile_attentions(vec![question(&project_id, "docs/x.md", "Q1")])
        .unwrap()
        .unwrap();
    assert_ne!(group2.id, group.id);
    assert_eq!(group2.generation, 1);
    assert_eq!(created2.len(), 1);
    assert_eq!(group2.state, "open");
}

#[test]
fn reconcile_followups_dedupe_on_proposed_name() {
    let (db, _product, _project_id, task_id) = fixture();
    let batch = || {
        vec![
            followup(&task_id, "wire retries"),
            followup(&task_id, "add metrics"),
        ]
    };
    let (group, created) = db.reconcile_attentions(batch()).unwrap().unwrap();
    assert_eq!(group.kind, "followup");
    assert_eq!(created.len(), 2);

    // Same names re-emitted with a different description still dedupe.
    let mut again = followup(&task_id, "wire retries");
    again.proposed_description = Some("reworded".to_owned());
    let (group2, created2) = db.reconcile_attentions(vec![again]).unwrap().unwrap();
    assert_eq!(group2.id, group.id);
    assert!(created2.is_empty());
}
