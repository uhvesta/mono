use std::sync::Arc;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::merge_poller::{CiProvider, OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
use crate::work::{CreateChoreInput, CreateProductInput, TaskStatus, WorkDb, WorkItem, WorkItemPatch};

#[derive(Default)]
struct RecordingPublisher {
    events: Mutex<Vec<(String, String, String)>>,
    typed_events: Mutex<Vec<(String, FrontendEvent)>>,
}

#[async_trait]
impl ExecutionPublisher for RecordingPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.events
            .lock()
            .await
            .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
    }
    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        self.typed_events.lock().await.push((product_id.to_owned(), event));
    }
}

fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
    let product = db
        .create_product(CreateProductInput {
            name: format!("Product-{name}"),
            description: None,
            repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (product.id, chore.id)
}

fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id: work_item_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: pr_url.to_owned(),
    }
}

fn probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe {
        url: pr_url.to_owned(),
        state: PrLifecycleState::Open(OpenPrStatus::clean()),
        base_ref_oid: Some("base-1".into()),
        head_ref_oid: Some(head_sha.to_owned()),
        head_ref_name: None,
        base_ref_name: None,
        labels: Vec::new(),
        review: crate::merge_poller::PrReviewState::Unknown,
        in_merge_queue: false,
    }
}

fn probe_with_labels(pr_url: &str, head_sha: &str, labels: &[&str]) -> PrLifecycleProbe {
    PrLifecycleProbe {
        url: pr_url.to_owned(),
        state: PrLifecycleState::Open(OpenPrStatus::clean()),
        base_ref_oid: Some("base-1".into()),
        head_ref_oid: Some(head_sha.to_owned()),
        head_ref_name: None,
        base_ref_name: None,
        labels: labels.iter().map(|s| (*s).to_owned()).collect(),
        review: crate::merge_poller::PrReviewState::Unknown,
        in_merge_queue: false,
    }
}

fn one_failure() -> Vec<RequiredCheckFailure> {
    vec![RequiredCheckFailure {
        name: "ci/test".into(),
        conclusion: "FAILURE".into(),
        target_url: "https://buildkite.com/anthropic/mono/builds/42#job-uuid".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("job-uuid".into()),
    }]
}

fn chore_state(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => (t.status, t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// The create-time revision gate's PR-state probe for tests. The
/// production CI producer feeds `StaticPrStateChecker(Open)` (the poller
/// just observed the PR open at clean mergeability); tests use the fake
/// so `create_revision`'s `assert_parent_revisable` sees an open PR
/// without a `gh` round-trip.
fn fix_checker() -> crate::work::FakePrStateChecker {
    crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
}

#[tokio::test]
async fn detection_flips_in_review_to_blocked_ci_failure() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10";
    let (product, chore) = make_in_review(&db, "C-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped, "first detection must flip the row");

    // In the in_review model a spawned revision immediately unblocks the
    // parent back to `in_review`; `blocked: ci_failure` is transient.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let events = pub_.events.lock().await.clone();
    assert!(events.iter().any(|(_, _, r)| r == "ci_revision_in_flight"));

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationStarted { .. }))
    );

    // Counter incremented by one because we created a fix-kind attempt.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

#[tokio::test]
async fn detection_is_idempotent_on_repeated_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/11";
    let (product, chore) = make_in_review(&db, "C-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let first = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let second = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(first);
    assert!(!second, "second probe with same head_sha must be a no-op");

    // Counter incremented exactly once across the duplicate probes.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

#[tokio::test]
async fn detection_defers_when_active_conflict_resolution_exists() {
    // §Q7 composed ordering: a conflict resolution attempt for
    // the same PR pre-empts the CI flow.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12";
    let (product, chore) = make_in_review(&db, "C-defer-cr", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 12,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("base-1".into()),
        head_sha_before: Some("head-1".into()),
    })
    .unwrap();
    // Reset to in_review so the WHERE guard would otherwise fire.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "active conflict-resolution must pre-empt CI flow");
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "row stays where it was");
}

#[tokio::test]
async fn detection_defers_when_active_rebase_attempt_exists() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/13";
    let (product, chore) = make_in_review(&db, "C-defer-rebase", pr);
    // Stand up the auto-rebase side table directly so the deferral
    // gate observes a non-terminal row.
    let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
    conn.execute(
        "CREATE TABLE rebase_attempts (
             id                TEXT PRIMARY KEY,
             dependent_pr_url  TEXT NOT NULL,
             status            TEXT NOT NULL
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
          VALUES ('reb_1', ?1, 'running')",
        [pr],
    )
    .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "active rebase attempt must pre-empt CI flow");
}

#[tokio::test]
async fn detection_lands_exhausted_when_budget_is_zero() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/14";
    let (product, chore) = make_in_review(&db, "C-exh", pr);
    // Set the per-product budget to 0 ("notify only").
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute("UPDATE products SET ci_attempt_budget = 0 WHERE id = ?1", [&product])
        .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationExhausted { .. }))
    );
    // No attempt row should have been inserted.
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

#[tokio::test]
async fn detection_skipped_when_pr_has_opt_out_label() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/15";
    let (product, chore) = make_in_review(&db, "C-optout", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(pr, "head-1", &["boss/no-auto-rebase"]),
        &one_failure(),
    )
    .await;
    assert!(!flipped);
}

#[tokio::test]
async fn detection_requires_head_ref_oid() {
    // Without `headRefOid` the engine can't key the attempt row,
    // so we leave the parent alone and wait for the next probe.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-no-head", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let mut p = probe(pr, "head-1");
    p.head_ref_oid = None;
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &p,
        &one_failure(),
    )
    .await;
    assert!(!flipped);
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
}

#[tokio::test]
async fn full_cycle_detect_then_retire() {
    // Probe → attempt → push (simulated) → next probe Clean → retire.
    // Idempotency: a second Clean probe is a no-op.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/17";
    let (product, chore) = make_in_review(&db, "C-cycle", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect.
    let detected = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(detected);
    // In the in_review model the parent stays in_review while the revision runs.
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);

    // 2. Retire — CI is back to clean.
    let resolved = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(resolved);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Attempt row terminal.
    let attempts: Vec<_> = {
        let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
        let mut stmt = conn
            .prepare("SELECT status FROM ci_remediations WHERE work_item_id = ?1")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([&chore], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    assert_eq!(attempts, vec!["succeeded".to_owned()]);

    // 3. Counter reset on successful cycle.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // 4. Repeat retire — no-op.
    let again = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(!again);
}

#[tokio::test]
async fn retire_skipped_when_product_opt_out_flag_disabled() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/18";
    let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detect first so there's something to retire.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
        [&product],
    )
    .unwrap();
    drop(conn);

    let retired = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(!retired, "opted-out product must not retire automatically");
    // In the in_review model the parent was never blocked; the retire
    // no-op leaves it in_review.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// When `on_ci_resolved` clears a `blocked: ci_failure` row but finds
/// no active (pending/running) remediation attempt — because the prior
/// attempt was already terminal (failed, abandoned) — it must emit
/// `CiFailureCleared` so the UI can clear its stale `ci failing` badge
/// without incorrectly setting the `ci auto-fixed` badge. (T606 fix)
#[tokio::test]
async fn retire_without_active_attempt_emits_ci_failure_cleared() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/19";
    let (product, chore) = make_in_review(&db, "C-no-active-attempt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect failure → attempt created and marked failed (simulating
    //    a worker that ran but couldn't push a fix).
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
        .unwrap();

    // 2. CI goes green on its own — no active attempt left.
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "attempt must be terminal before retire"
    );
    let resolved = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(resolved, "retire must succeed even without active attempt");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Engine must emit CiFailureCleared (not CiRemediationSucceeded)
    // so the UI clears the failure badge without setting auto-fixed.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must be emitted when task clears without active attempt"
    );
    assert!(
        !typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationSucceeded { .. })),
        "CiRemediationSucceeded must NOT be emitted when there is no active attempt"
    );
}

/// Issue #901: a chore left in `blocked: ci_failure` from a prior
/// run is superseded once CI re-enters InFlight (no active
/// remediation). The chore returns to `in_review`, `CiFailureCleared`
/// is emitted so the UI drops the stale badge, and the CI budget
/// counter is preserved (the run hasn't passed yet).
#[tokio::test]
async fn in_flight_supersedes_stale_ci_failure() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/901";
    let (product, chore) = make_in_review(&db, "C-supersede", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect failure → blocked: ci_failure, budget=1, attempt
    //    created. Then mark the attempt failed so no active
    //    remediation remains (a worker that ran but couldn't push).
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
        .unwrap();
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // 2. CI re-runs (InFlight) — the stale failure is superseded.
    // The attempt was marked failed, so active_ci_remediation returns None;
    // any head SHA passes.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-1"),
    )
    .await;
    assert!(cleared, "stale ci_failure must be superseded by InFlight");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must drop the stale badge",
    );
    let events = pub_.events.lock().await.clone();
    assert!(events.iter().any(|(_, _, r)| r == "ci_failure_superseded_in_progress"),);

    // Budget is NOT reset — the re-run hasn't passed yet, so a fresh
    // failure must keep consuming the remaining allotment.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

/// An *active* remediation attempt owns the slot: its own fix push is
/// what re-triggered CI, so its in-flight chip must not be cleared.
/// The supersede path declines and the chore stays blocked.
#[tokio::test]
async fn in_flight_supersede_skips_when_active_remediation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/902";
    let (product, chore) = make_in_review(&db, "C-active-rem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detection leaves a pending (active) remediation attempt.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_some(),
        "attempt must be active before the supersede check",
    );

    // Same head SHA as the active remediation → the fix worker's own re-run; must not supersede.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-1"),
    )
    .await;
    assert!(!cleared, "active remediation for same head must not be superseded");

    // In the in_review model the parent stays in_review while the revision runs.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// No stale failure to supersede (chore already `in_review`): the
/// supersede path is a cheap WHERE-guard no-op and emits nothing.
#[tokio::test]
async fn in_flight_supersede_noop_when_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/903";
    let (product, chore) = make_in_review(&db, "C-noop", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let cleared =
        on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[], None).await;
    assert!(!cleared, "an in_review chore has no stale failure to clear");

    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(pub_.typed_events.lock().await.is_empty());
    assert!(pub_.events.lock().await.is_empty());
}

/// The opt-out label suppresses the supersede just like the detect /
/// retire paths: a stale ci_failure on an opted-out PR is left alone.
#[tokio::test]
async fn in_flight_supersede_skipped_when_pr_has_opt_out_label() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/904";
    let (product, chore) = make_in_review(&db, "C-supersede-optout", pr);
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());

    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &["boss/no-auto-rebase".to_owned()],
        Some("head-1"),
    )
    .await;
    assert!(!cleared, "opt-out label must suppress the supersede");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));
}

/// First InFlight probe records `first_observed_at` but emits
/// nothing (no threshold crossed). A subsequent probe whose
/// observed timestamp is rewound by >30min lands in the `warn`
/// bucket; rewinding past 2h lands in `alert`. Repeated probes at
/// the same bucket are no-ops (the WHERE guard rejects same-level
/// re-emits).
#[tokio::test]
async fn never_starts_alert_crosses_warn_then_alert() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/30";
    let (product, chore) = make_in_review(&db, "C-never-starts", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Probe #1: no threshold crossed.
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "none");
    let typed_after_first = pub_.typed_events.lock().await.clone();
    assert!(typed_after_first.is_empty(), "no event before any bucket");

    // Rewind the observation timestamp by 31 min so the next probe
    // crosses the warn threshold.
    let warn_cutoff = current_unix_secs() - (31 * 60);
    rewind_inflight_observation(&db_path, &chore, "head-A", warn_cutoff);
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "warn");
    // Still no soft-alert frontend event — warn is log-only.
    let typed_after_warn = pub_.typed_events.lock().await.clone();
    assert!(
        typed_after_warn
            .iter()
            .all(|(_, ev)| !matches!(ev, FrontendEvent::CiNeverStartsAlert { .. })),
        "warn bucket must not emit CiNeverStartsAlert event",
    );

    // A second probe at the same elapsed bucket is a no-op (the
    // alert-level WHERE guard rejects a same-level rewrite).
    let again = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(again, "warn");

    // Rewind past 2h so the next probe upgrades to alert.
    let alert_cutoff = current_unix_secs() - (2 * 60 * 60 + 60);
    rewind_inflight_observation(&db_path, &chore, "head-A", alert_cutoff);
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "alert");
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiNeverStartsAlert {
                level,
                ..
            } if level == "2h"
        )),
        "alert bucket must emit CiNeverStartsAlert with level=2h",
    );
}

/// A fresh push (new head sha) keys observations on its own row,
/// so the timer restarts from zero and the previous bucket doesn't
/// carry over.
#[tokio::test]
async fn never_starts_alert_resets_on_new_head_sha() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/31";
    let (product, chore) = make_in_review(&db, "C-new-head", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Drive head-A all the way to `alert`.
    on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    rewind_inflight_observation(&db_path, &chore, "head-A", current_unix_secs() - (3 * 60 * 60));
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "alert");

    // A new head sha starts fresh.
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-B"),
    )
    .await;
    assert_eq!(level, "none", "new head sha must reset the timer");
}

/// When the engine flips the chore to `blocked: ci_failure` (CI
/// transitions from InFlight to Failing), the leftover observation
/// row must be cleared so a later InFlight stretch starts fresh.
#[tokio::test]
async fn detection_clears_inflight_observation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/32";
    let (product, chore) = make_in_review(&db, "C-clear-on-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
    )
    .await;
    let n: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
            [&chore],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "observation row exists after InFlight probe");

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let n: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
            [&chore],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "Failing detection must clear inflight observations");
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Rewrite the `first_observed_at` timestamp on a
/// `ci_inflight_observations` row to simulate the passage of time
/// without sleeping. Used by the never-starts-alert tests.
fn rewind_inflight_observation(db_path: &std::path::Path, work_item_id: &str, head_sha: &str, when_unix_secs: i64) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE ci_inflight_observations
            SET first_observed_at = ?3
          WHERE work_item_id = ?1 AND head_sha = ?2",
        rusqlite::params![work_item_id, head_sha, when_unix_secs.to_string()],
    )
    .unwrap();
}

/// Regression for the operator-reported "stale badge from prior run" scenario:
/// a push to the PR changes the head SHA while the prior run's `ci_remediations`
/// row is still `pending`. The new CI run is all-in-flight (no failing leaf).
///
/// Before the fix, `on_ci_in_flight_supersedes_failure` bailed when it found the
/// pending row (even though it was for the old head SHA), leaving the macOS badge
/// stuck at "ci failing". After the fix the stale row is abandoned and
/// `CiFailureCleared` is emitted so the badge correctly reflects the new run.
///
/// This is the scenario the operator described: "they were all in progress, but it
/// was showing a stale badge. I don't think the original shake was actually based
/// on things that had one test failing."
#[tokio::test]
async fn new_commit_all_inflight_abandons_stale_remediation_and_clears_badge() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1160";
    let (product, chore) = make_in_review(&db, "C-stale-badge", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // --- Step 1: Prior commit (head-A) terminally fails CI and a remediation is created. ---
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
        &one_failure(),
    )
    .await;

    let prior_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending remediation row must exist after detection");
    assert_eq!(
        prior_attempt.head_sha_at_trigger, "head-A",
        "remediation row must record the head SHA at trigger",
    );
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // --- Step 2: User pushes a new commit (head-B). GitHub restarts CI
    // from scratch — all checks are now queued / running (InFlight).
    // NO failing leaf in this new rollup: this is the all-in-progress case.
    // The prior remediation row is still `pending` (the fix worker hasn't
    // done anything yet — the push made its revision moot). ---
    pub_.events.lock().await.clear();
    pub_.typed_events.lock().await.clear();

    let superseded = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-B"), // new head SHA — DIFFERENT from the pending row's head_sha_at_trigger
    )
    .await;

    assert!(
        superseded,
        "InFlight at a new head SHA must supersede the stale remediation and clear the badge",
    );

    // The stale row must be abandoned — not terminal-failed, not pending.
    let still_active = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        still_active.is_none(),
        "the stale remediation row must be abandoned, not left pending",
    );

    // `CiFailureCleared` must be emitted so the macOS badge clears.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must be emitted when stale remediation is superseded by a new head",
    );

    // Budget counter is NOT reset — the new run hasn't passed yet.
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        1,
        "budget counter must not reset until CI actually passes",
    );

    // --- Step 3: same-head-SHA guard still holds — a fix worker's own CI re-run
    // at the SAME head SHA must NOT be superseded (or the badge would vanish while
    // the fix is running). Create a fresh remediation for head-C and then probe
    // InFlight at head-C — should NOT supersede. ---
    pub_.events.lock().await.clear();
    pub_.typed_events.lock().await.clear();

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-C"),
        &one_failure(),
    )
    .await;

    let not_superseded = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-C"), // SAME head SHA as the active remediation
    )
    .await;

    assert!(
        !not_superseded,
        "active remediation for the same head SHA must NOT be superseded (fix worker's own run)",
    );

    let typed_after = pub_.typed_events.lock().await.clone();
    assert!(
        !typed_after
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiFailureCleared { .. })),
        "CiFailureCleared must NOT be emitted when the active remediation is for the same head",
    );
}

#[test]
fn encode_failed_checks_round_trip() {
    let json = super::encode_failed_checks(&[RequiredCheckFailure {
        name: "ci/test".into(),
        conclusion: "FAILURE".into(),
        target_url: "https://github.com/foo/bar/actions/runs/1/job/2".into(),
        provider: CiProvider::GithubActions,
        provider_job_id: Some("2".into()),
    }]);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let item = &arr[0];
    assert_eq!(item["name"], "ci/test");
    assert_eq!(item["provider"], "github_actions");
    assert_eq!(item["provider_job_id"], "2");
}

// ----- Phase 9 #28: pre-triage classification permutations ----------

fn failure(name: &str, conclusion: &str) -> RequiredCheckFailure {
    RequiredCheckFailure {
        name: name.into(),
        conclusion: conclusion.into(),
        target_url: "https://buildkite.com/foo/bar/builds/1#x".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("x".into()),
    }
}

#[test]
fn pre_triage_all_startup_failure_routes_to_retrigger() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "STARTUP_FAILURE")];
    assert_eq!(super::classify_pre_triage(&fs), "retrigger");
}

#[test]
fn pre_triage_mixed_startup_and_cancelled_routes_to_retrigger() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "CANCELLED")];
    assert_eq!(super::classify_pre_triage(&fs), "retrigger");
}

#[test]
fn pre_triage_one_real_failure_routes_to_fix() {
    let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "FAILURE")];
    assert_eq!(super::classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_all_failure_routes_to_fix() {
    let fs = [failure("a", "FAILURE"), failure("b", "TIMED_OUT")];
    assert_eq!(super::classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_action_required_routes_to_fix() {
    // ACTION_REQUIRED isn't unambiguous infra — it needs a human or
    // a worker triage decision, so it stays on the fix path.
    let fs = [failure("a", "ACTION_REQUIRED")];
    assert_eq!(super::classify_pre_triage(&fs), "fix");
}

#[test]
fn pre_triage_empty_defaults_to_fix() {
    assert_eq!(super::classify_pre_triage(&[]), "fix");
}

// ----- Phase 4 cutover: engine-triggered revision as the fix vehicle -----

#[tokio::test]
async fn detection_spawns_revision_and_stamps_attempt() {
    // A genuinely-new `fix`-kind CI failure creates a `kind=revision`
    // task (parent = chore, ci-fix provenance), stamps the ledger row's
    // `revision_task_id`, and creates NO bespoke ci_remediation
    // execution — the dormant path stays dormant and the row is hidden
    // from the rescue recovery query.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/100";
    let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.attempt_kind, "fix");
    let rev_id = attempt
        .revision_task_id
        .clone()
        .expect("the producer must stamp revision_task_id on the attempt");

    let revision = match db.get_work_item(&rev_id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
    assert_eq!(revision.created_via, format!("ci-fix:{}", attempt.id));
    assert_eq!(revision.description, "Fix failing CI: ci/test");

    // No bespoke ci_remediation execution: the revision rides the
    // reconcile loop's revision_implementation dispatch instead.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = ?2",
            rusqlite::params![&chore, "ci_remediation"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "cutover must not create a ci_remediation execution");

    // The revision-backed row is invisible to the dormant rescue path.
    assert!(
        db.list_stranded_ci_remediation_attempts().unwrap().is_empty(),
        "revision-backed attempt must be excluded from the rescue query",
    );
}

#[tokio::test]
async fn detection_idempotent_does_not_double_spawn_revision() {
    // Re-firing on the same head sha reuses the existing attempt (whose
    // revision_task_id is already set) and spawns no second revision.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/101";
    let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    // Reset to in_review so the second probe re-enters the primary flip
    // path with the same head sha (UNIQUE collision on the ledger).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;

    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same head sha must not stack attempts");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 1, "same head sha must not stack revisions");
}

#[tokio::test]
async fn retrigger_creates_bespoke_execution_and_no_revision() {
    // `retrigger` produces no commit, so it stays on the bespoke
    // ci_remediation execution kind (design Q6) and never spawns a
    // revision or consumes budget.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/102";
    let (product, chore) = make_in_review(&db, "C-retrigger", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // All-infra failures classify as `retrigger`.
    let infra = vec![failure("ci/flaky", "STARTUP_FAILURE")];
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &infra,
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.attempt_kind, "retrigger");
    assert!(
        attempt.revision_task_id.is_none(),
        "retrigger must not spawn a revision",
    );

    // Exactly one bespoke ci_remediation execution; no revision task.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 1, "retrigger must park a ci_remediation execution");
    let revisions: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks WHERE kind = 'revision'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revisions, 0, "retrigger must not create a revision");

    // Retrigger does not consume the fix budget.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}

// ----- Reconciled 2026-05-17 layered design call: rebase-first success ----

#[tokio::test]
async fn rebase_only_success_refunds_budget_slot() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/200";
    let (product, chore) = make_in_review(&db, "C-rebase", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect a fix-kind failure — counter bumps to 1.
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped);
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // 2. Worker rebases onto base HEAD and reports green CI without
    //    a code change: rebase-only success path.
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    let updated = db
        .mark_ci_remediation_succeeded_via_rebase(&attempt.id)
        .unwrap()
        .expect("WHERE guard hit");

    assert_eq!(updated.status, "succeeded");
    assert_eq!(updated.consumes_budget, 0);
    assert_eq!(updated.failure_reason.as_deref(), Some("rebase_only"));

    // 3. Counter refunded: budget slot is NOT consumed.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // 4. Idempotent — repeat is a no-op.
    let again = db.mark_ci_remediation_succeeded_via_rebase(&attempt.id).unwrap();
    assert!(again.is_none(), "second call must be a no-op");
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}

// ----- Merge-queue rebounce detection (T605 regression, PR #690) -----

/// A PR whose head-branch CI is all green but that was removed from
/// the merge queue with `reason=FAILED_CHECKS` must flip its owning
/// chore to `blocked: ci_failure` and park a `ci_remediation` execution.
///
/// This is the basic reproducer for the T604 missed-detection: the
/// engine must act on the `RemovedFromMergeQueueEvent` timeline signal,
/// not on the per-PR `statusCheckRollup` (which stays SUCCESS after a
/// dequeue).
#[tokio::test]
async fn rebounce_flips_in_review_to_blocked_ci_failure() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/500";
    let (product, chore) = make_in_review(&db, "C-rebounce-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        None,
        "synthetic-merge-sha-abc",
        &[],
    )
    .await;
    assert!(flipped, "rebounce detection must flip chore to ci_failure");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));

    // Phase 5 cutover: no bespoke ci_remediation execution — the fix
    // delivers via an engine-triggered revision instead.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 1, "rebounce must spawn exactly one revision task");

    // The ci_remediations row must record the failure as a queue rebounce
    // and have its revision_task_id stamped.
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");
    assert_eq!(attempt.failure_kind.as_deref(), Some("merge_queue_rebounce"));
    assert_eq!(attempt.before_commit_sha.as_deref(), Some("synthetic-merge-sha-abc"));
    assert!(
        attempt.revision_task_id.is_some(),
        "attempt must have revision_task_id stamped"
    );
}

/// THE REGRESSION (T604 / PR #690 04:44Z miss): a clean head-branch CI
/// probe must NOT clear a `merge_queue_rebounce` block.
///
/// Before the fix, `on_ci_resolved` treated "head-branch CI is green" as
/// a sufficient clearing signal for ALL ci_failure reasons.  For a
/// rebounce, the PR's own CI is *always* green (the failure is on the
/// synthetic merge commit), so every sweep immediately un-blocked the
/// chore, preventing detection from sticking.
#[tokio::test]
async fn rebounce_block_not_cleared_by_clean_head_branch_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/501";
    let (product, chore) = make_in_review(&db, "C-rebounce-noclr", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: detect the rebounce — chore flips to blocked: ci_failure.
    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        None,
        "synthetic-sha-xyz",
        &[],
    )
    .await;
    assert!(flipped);

    // Step 2: simulate the merge_poller's next sweep — the head-branch CI
    // probe returns Clean (statusCheckRollup is all SUCCESS), so sweep_one
    // calls on_ci_resolved.  This must NOT clear the rebounce block.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(
        !cleared,
        "on_ci_resolved must not clear a merge_queue_rebounce block based on \
         head-branch CI; the PR's own CI is always green in this case"
    );

    // Chore must still be blocked after the clean probe.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));
}

/// Defect #3 (the un-block side fighting the block): an InFlight head-branch
/// CI probe must NOT clear a `merge_queue_rebounce` block.
///
/// The PR's own branch CI is green for a queue failure, and the rebounce
/// attempt's `head_sha_at_trigger` is the synthetic merge commit — which never
/// equals the PR head — so `on_ci_in_flight_supersedes_failure`'s stale-head
/// heuristic would otherwise read "stale", abandon the attempt, and clear the
/// block. The next sweep's rebounce check would re-block it: the observed
/// blocked<->in_review flap. The block must stand and the attempt must stay
/// pending (so `on_ci_resolved`'s guard keeps holding too).
#[tokio::test]
async fn rebounce_block_not_cleared_by_inflight_head_branch_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/504";
    let (product, chore) = make_in_review(&db, "C-rebounce-inflight", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        None,
        "synthetic-sha-inflight",
        &[],
    )
    .await;
    assert!(flipped);

    // Merge poller's next sweep probes the PR head and finds CI InFlight.
    // `current_head_sha` (PR head) differs from the synthetic merge SHA the
    // attempt was keyed on — exactly the condition that used to mis-fire.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("pr-head-sha-different"),
    )
    .await;
    assert!(
        !cleared,
        "InFlight head-branch CI must not supersede a merge_queue_rebounce block",
    );

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("rebounce attempt must still be active (not abandoned by the supersede path)");
    assert_eq!(attempt.failure_kind.as_deref(), Some("merge_queue_rebounce"));
}

/// End-to-end anti-flap reproducer: across repeated sweeps of an UNCHANGED
/// failing merge SHA — with both an InFlight supersede probe and a Clean
/// `on_ci_resolved` probe running between rebounce checks every cycle — the
/// chore bounces to blocked AT MOST ONCE and never oscillates back to
/// in_review. This is the operator-reported symptom (~once-a-minute flap)
/// pinned shut: defects #1 (per-sha idempotency) and #3 (sticky block) acting
/// together.
#[tokio::test]
async fn rebounce_does_not_flap_across_repeated_sweeps() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/505";
    let (product, chore) = make_in_review(&db, "C-rebounce-noflap", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);
    let sha = "synthetic-merge-noflap";

    let mut bounce_count = 0;
    for cycle in 0..5 {
        // The rebounce pass re-sees the same dequeue event on every sweep.
        if on_merge_queue_rebounce_detected(&db, pub_.as_ref(), &cand, Some("feature"), None, sha, &[]).await {
            bounce_count += 1;
        }
        // The per-PR probe alternates between InFlight (supersede) and Clean
        // (on_ci_resolved) — both opposing un-block paths must decline.
        on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("pr-head")).await;
        on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await;

        // Invariant on every cycle after the first bounce: still blocked.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(
            status,
            TaskStatus::Blocked,
            "must stay blocked on cycle {cycle} (no flap)"
        );
        assert_eq!(reason.as_deref(), Some("ci_failure"), "cycle {cycle}");
    }
    assert_eq!(
        bounce_count, 1,
        "an unchanged failing merge SHA must bounce exactly once across all sweeps"
    );
}

/// A second probe of the same dequeue event (same `before_commit_sha`)
/// is idempotent: the INSERT OR IGNORE is a no-op, but the chore stays
/// blocked and no new execution is created.
#[tokio::test]
async fn rebounce_detection_idempotent_on_same_sha() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/502";
    let (product, chore) = make_in_review(&db, "C-rebounce-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let first = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-A",
        &[],
    )
    .await;
    // Repeat for the same SHA (as would happen when the same dequeue event
    // appears in the timeline across consecutive sweeps).
    let second = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-A",
        &[],
    )
    .await;
    assert!(first, "first detection must flip the chore");
    assert!(!second, "second probe for same SHA must be a no-op");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));

    // Phase 5 cutover: exactly one revision, no ci_remediation executions.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rev_count, 1,
        "exactly one revision; duplicate probe must not spawn a second"
    );
}

/// After the worker marks the attempt succeeded, the next `on_ci_resolved`
/// call (with clean head-branch CI) should clear the rebounce block — that
/// is the correct terminal path.
#[tokio::test]
async fn rebounce_block_clears_after_worker_succeeds() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/503";
    let (product, chore) = make_in_review(&db, "C-rebounce-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect.
    on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-Q",
        &[],
    )
    .await;

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");

    // 2. Worker marks attempt succeeded (re-enqueued the PR).
    db.mark_ci_remediation_succeeded(&attempt.id, None)
        .unwrap()
        .expect("succeeded update");

    // 3. Now on_ci_resolved fires (head-branch CI still clean) — no active
    //    attempt exists, so the rebounce guard does not fire and the block
    //    is cleared correctly.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(cleared, "after worker succeeds, on_ci_resolved must clear the block");

    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
}

// ----- Back-to-back dequeue regression (T628 / PR #718 06:51Z miss) -----

/// Reproducer for T628: a PR that was dequeued, manually re-queued,
/// and dequeued again must end up with a parked revision for the second
/// dequeue's SHA — without requiring the first dequeue's worker to have
/// completed.
///
/// This also pins the anti-flap contract: replaying an ALREADY-handled
/// dequeue SHA must NOT re-bounce the chore. The merge-queue dequeue event
/// stays in the PR timeline forever, so a resolved SHA would otherwise
/// re-block on every sweep (the blocked<->in_review flap). Only a genuinely
/// new failing merge SHA may flip an `in_review` chore back to blocked.
///
/// Sequence:
///   1. Chore in_review; first dequeue (SHA_1) detected → blocked, revision-1 spawned.
///   2. Worker marks SHA_1 succeeded_via_rebase (human re-queued the PR).
///   3. on_ci_resolved clears the block → chore back to in_review.
///   4. Next sweep sees both SHA_1 and SHA_2 in the timeline:
///      - SHA_1: INSERT IGNORED (key exists, row terminal) → per-sha
///               idempotency returns false; chore STAYS in_review (no re-bounce).
///      - SHA_2: INSERT succeeds → fresh attempt; chore is in_review so
///               mark_chore_blocked_ci_failure flips it to blocked and the
///               attempt gets its own revision immediately.
///   5. End state: chore blocked on SHA_2, exactly two revisions, nothing
///      stranded — and SHA_1's stale dequeue never caused a flap.
///
/// Detection must not require a live worker on the chore.
#[tokio::test]
async fn back_to_back_rebounce_parks_execution_for_second_dequeue() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/718";
    let (product, chore) = make_in_review(&db, "C-t628-backtoback", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first dequeue (SHA_1) → chore flips to blocked, revision spawned.
    let first = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-merge-1",
        &[],
    )
    .await;
    assert!(first, "first rebounce must flip chore to ci_failure");
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));
    {
        // Phase 5 cutover: no bespoke ci_remediation execution; a revision is
        // spawned instead.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions
                  WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "cutover: no ci_remediation execution after first dequeue");
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 1, "exactly one revision after first dequeue");
    }

    // Step 2: mark SHA_1's ci_remediations row succeeded_via_rebase (PR re-queued
    // by human). In production a revision_implementation worker does the push and
    // the poller retires the ledger row; here we use the DB helper directly.
    let sha1_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("sha1 attempt row");
    db.mark_ci_remediation_succeeded_via_rebase(&sha1_attempt.id)
        .unwrap()
        .expect("succeeded_via_rebase update");

    // Step 3: on_ci_resolved clears the block → chore in_review again.
    let cleared = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(cleared, "on_ci_resolved must clear the block after SHA_1 is terminal");
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);

    // Step 4a: next sweep replays SHA_1 — INSERT is ignored (key exists, row
    // terminal). Per-sha idempotency: we already bounced (and resolved) this
    // failing merge SHA, so this is a no-op. The chore must NOT re-bounce — a
    // resolved dequeue event re-blocking on every sweep is the flap this fix
    // eliminates. The chore stays in_review.
    let sha1_replay = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-merge-1",
        &[],
    )
    .await;
    assert!(
        !sha1_replay,
        "sha1 replay must be an idempotent no-op (already bounced + resolved); no re-flip"
    );
    let (status, _reason) = chore_state(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "resolved SHA_1 replay must leave the chore in_review (no flap)"
    );
    // Still just the original revision from step 1.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 1, "sha1 replay must not spawn a second revision");
    }

    // Step 4b: same sweep also sees SHA_2 — a genuinely NEW failing merge SHA.
    // INSERT succeeds (new key); the chore is in_review (SHA_1 replay was a
    // no-op), so mark_chore_blocked_ci_failure flips it to blocked and the
    // fresh attempt gets its own revision immediately.
    let sha2_detect = on_merge_queue_rebounce_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        None,
        "sha-merge-2",
        &[],
    )
    .await;
    assert!(
        sha2_detect,
        "sha2 is a new failing merge SHA — it must bounce the in_review chore to blocked"
    );
    // SHA_2's ci_remediations row must exist as pending with revision_task_id stamped.
    let sha2_attempt = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ci_remediations
                  WHERE work_item_id = ?1 AND head_sha_at_trigger = 'sha-merge-2'
                    AND status = 'pending'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "sha2 ci_remediations row must be pending");
        db.active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("sha2 attempt row")
    };
    assert!(
        sha2_attempt.revision_task_id.is_some(),
        "sha2 attempt must have a revision immediately — no stranding"
    );
    // Two revisions total: one for SHA_1, one for SHA_2.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r, 2, "sha2 must have its own revision; total revisions must be 2");
    }
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));

    // Sanity: no stranded ci_remediation attempts — sha2 has revision_task_id.
    let stranded = db.list_stranded_ci_remediation_attempts().unwrap();
    assert!(
        stranded.is_empty(),
        "no stranded attempts: sha2 has revision_task_id so it is excluded from rescue"
    );
}

#[tokio::test]
/// Regression guard for PR #1404 / issue T1431: a new CI-fix revision must
/// not spawn while a prior attempt's revision worker is still in flight (status
/// `todo` or `active`), even when the `ci_remediations` row was prematurely
/// retired by `ci_attempt_signal_cleared` (the originally-failing checks are
/// no longer in the failing set after a flaky re-trigger, while the worker has
/// not pushed a fix commit yet).
async fn detection_defers_when_prior_ci_fix_revision_still_in_flight() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/200";
    let (product, chore) = make_in_review(&db, "C-overlap-guard", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first CI failure on head-1 → attempt A, revision R1.
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped, "first detection must transition the chore");

    let attempt_a = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt A must exist");
    let rev_id = attempt_a
        .revision_task_id
        .clone()
        .expect("revision_task_id must be stamped");

    // Step 2: simulate premature retirement of attempt A — the originally-
    // failing checks are no longer in the failure set (e.g. a re-triggered
    // flaky check now passes while R1's worker is still running with no push).
    db.mark_ci_remediation_succeeded(&attempt_a.id, None).unwrap();

    // Verify R1 is still `todo` (worker has not started yet / no push).
    let rev_task = match db.get_work_item(&rev_id).unwrap() {
        crate::work::WorkItem::Task(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    assert_eq!(
        rev_task.status,
        TaskStatus::Todo,
        "R1 must still be todo — worker has not pushed",
    );

    // Verify primary gate is now bypassed (no active ci_remediations row).
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "primary gate bypassed: no active ci_remediations row",
    );

    // Step 3: chore moves back to in_review (as it would be after
    // unblock_for_revision on a CI that appears clean momentarily).
    db.update_work_item(
        &chore,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    // Step 4: CI is still failing (perhaps different checks). Without the
    // secondary pre-flight guard this would spawn a second revision while R1
    // is still in flight. With the guard, spawning must be deferred.
    let flipped2 = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"), // same head — same failing SHA
        &one_failure(),
    )
    .await;
    assert!(
        !flipped2,
        "second detection must be deferred while R1 is still in flight (todo)",
    );

    // Only one ci_remediations row and one revision must exist.
    let all_attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all_attempts.len(), 1, "must not create a second attempt row");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 1, "must not spawn a second revision while R1 is active");
}

#[tokio::test]
/// After R1 pushes (moves to `in_review`) and CI fails on the pushed commit,
/// a new attempt IS allowed — the previous worker completed its job.
async fn detection_allowed_after_prior_revision_pushes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/201";
    let (product, chore) = make_in_review(&db, "C-overlap-after-push", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first CI failure → attempt A, revision R1.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;

    let attempt_a = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt A");
    let rev_id = attempt_a.revision_task_id.clone().expect("revision_task_id");

    // Step 2: R1 pushes a commit (moves to in_review) and the
    // ci_remediations row is marked succeeded (CI went green momentarily,
    // then a new failure emerged on head-2).
    db.update_work_item(
        &rev_id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    db.mark_ci_remediation_succeeded(&attempt_a.id, Some("head-2")).unwrap();

    // Chore returns to in_review.
    db.update_work_item(
        &chore,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    // Step 3: CI is failing on head-2 (R1's pushed commit). R1 is in_review
    // (not todo/active), so the secondary guard must NOT defer — a new attempt
    // for head-2 is the correct outcome.
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "R1 in in_review must not count as in-flight",
    );

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-2"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "detection must proceed after R1 pushed — the prior worker completed its job",
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 2, "second revision must be spawned for the new CI failure");
}
