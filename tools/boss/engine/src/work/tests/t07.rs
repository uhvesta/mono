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
    assert_eq!(matches[0].owner.kind, "chore");
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
