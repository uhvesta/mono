// Behavior 8 (title/body drift) tests, extracted to keep reconcile/mod.rs
// under the 3000-line file-size limit.
//
// All test helpers (in_memory_db, spy_registry, SpyTracker, …) live in the
// parent `tests` module and are reachable via `super::`.

use std::sync::Arc;

use boss_protocol::WorkItemPatch;
use crate::metrics::Registry;

use super::super::{register_metrics, run_one_pass};

/// Behavior 8 auto-sync: upstream changes title and body, boss side is
/// unchanged → boss row is updated from upstream.
#[tokio::test]
async fn b8_auto_syncs_when_only_upstream_changed() {
    let db = super::in_memory_db();
    let product = super::setup_product_with_tracker(&db);

    // Tick 1: import the item.
    let tracker = super::SpyTracker::new(vec![super::open_item(100, "Original Title")]);
    let registry = super::spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);
    let outcome1 =
        run_one_pass(&db, &registry, &metrics, &super::noop_pub(), &super::ambient_resolver())
            .await;
    assert_eq!(outcome1.items_imported, 1, "tick 1 should import the item");

    let task = db.find_by_external_ref("spy", "spy#100").unwrap().unwrap();
    assert_eq!(task.name, "Original Title");
    assert!(task.description.contains("Body of issue 100"));

    // Tick 2: upstream changes title and body; boss side was not edited.
    let mut updated = super::open_item(100, "Updated Title");
    updated.body = "Updated body text".to_owned();
    let tracker2 = super::SpyTracker::new(vec![updated]);
    let registry2 = super::spy_registry(tracker2);
    let metrics2 = Registry::new();
    register_metrics(&metrics2);
    let publisher = Arc::new(super::RecordingPublisher::default());
    let outcome2 =
        run_one_pass(&db, &registry2, &metrics2, publisher.as_ref(), &super::ambient_resolver())
            .await;

    assert_eq!(outcome2.title_body_synced, 1, "should auto-sync");
    assert_eq!(outcome2.title_body_conflict, 0);
    assert_eq!(outcome2.items_imported, 0, "no new import on tick 2");

    let synced = db.find_by_external_ref("spy", "spy#100").unwrap().unwrap();
    assert_eq!(synced.name, "Updated Title", "name should be synced from upstream");
    assert!(
        synced.description.contains("Updated body text"),
        "description should contain new body"
    );
    assert!(
        synced.description.starts_with("> Imported from"),
        "Imported from breadcrumb must be preserved"
    );

    let calls = publisher.recorded();
    assert!(
        calls.iter().any(|(_, _, r)| r == "chore_updated"),
        "chore_updated invalidation should have fired"
    );
    let _ = product;
}

/// Behavior 8 conflict: both upstream and boss were edited since import
/// → the reconciler logs a warning but does NOT overwrite the boss edits.
#[tokio::test]
async fn b8_skips_sync_when_both_sides_changed() {
    let db = super::in_memory_db();
    let product = super::setup_product_with_tracker(&db);

    // Tick 1: import the item.
    let tracker = super::SpyTracker::new(vec![super::open_item(101, "Original Title")]);
    let registry = super::spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);
    run_one_pass(&db, &registry, &metrics, &super::noop_pub(), &super::ambient_resolver()).await;

    let task = db.find_by_external_ref("spy", "spy#101").unwrap().unwrap();
    assert_eq!(task.name, "Original Title");

    // Simulate an operator edit of the task name on the boss side.
    db.update_task(
        &task.id,
        WorkItemPatch { name: Some("Operator Title".to_owned()), ..Default::default() },
        "human",
    )
    .expect("update_task");

    // Tick 2: upstream ALSO changes the title → conflict.
    let tracker2 = super::SpyTracker::new(vec![super::open_item(101, "Upstream Changed Title")]);
    let registry2 = super::spy_registry(tracker2);
    let metrics2 = Registry::new();
    register_metrics(&metrics2);
    let outcome2 =
        run_one_pass(&db, &registry2, &metrics2, &super::noop_pub(), &super::ambient_resolver())
            .await;

    assert_eq!(outcome2.title_body_conflict, 1, "should detect conflict");
    assert_eq!(outcome2.title_body_synced, 0, "should NOT auto-sync");

    // Boss name must be preserved as-is.
    let after = db.find_by_external_ref("spy", "spy#101").unwrap().unwrap();
    assert_eq!(after.name, "Operator Title", "operator edit must be preserved");
    let _ = product;
}

/// Behavior 8 no-op: nothing changed between ticks → no sync, no conflict.
#[tokio::test]
async fn b8_no_op_when_nothing_changed() {
    let db = super::in_memory_db();
    super::setup_product_with_tracker(&db);

    // Tick 1: import.
    let tracker = super::SpyTracker::new(vec![super::open_item(102, "Stable Title")]);
    let registry = super::spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);
    run_one_pass(&db, &registry, &metrics, &super::noop_pub(), &super::ambient_resolver()).await;

    // Tick 2: same upstream data — nothing should happen.
    let tracker2 = super::SpyTracker::new(vec![super::open_item(102, "Stable Title")]);
    let registry2 = super::spy_registry(tracker2);
    let metrics2 = Registry::new();
    register_metrics(&metrics2);
    let outcome2 =
        run_one_pass(&db, &registry2, &metrics2, &super::noop_pub(), &super::ambient_resolver())
            .await;

    assert_eq!(outcome2.title_body_synced, 0, "nothing to sync");
    assert_eq!(outcome2.title_body_conflict, 0, "no conflict");
}

/// Behavior 8 import baseline: items imported via `import_chore_with_external_ref`
/// have their drift-detection checksums stored so a second-tick no-op is correct.
#[tokio::test]
async fn b8_import_stores_upstream_baseline() {
    use crate::work::content_checksum;

    let db = super::in_memory_db();
    super::setup_product_with_tracker(&db);

    // Tick 1 imports the item and seeds the baseline.
    let tracker = super::SpyTracker::new(vec![super::open_item(103, "Baseline Title")]);
    let registry = super::spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);
    run_one_pass(&db, &registry, &metrics, &super::noop_pub(), &super::ambient_resolver()).await;

    // Confirm the checksum columns were written during import.
    let task = db.find_by_external_ref("spy", "spy#103").unwrap().unwrap();
    let stored = db.reconciler_get_content_checksums(&task.id).unwrap();
    assert!(stored.is_some(), "content checksums should be set after import");
    let (upstream_checksum, _boss_checksum) = stored.unwrap();
    assert_eq!(
        upstream_checksum,
        content_checksum("Baseline Title", "Body of issue 103"),
        "upstream checksum should match SHA-256 of canonical title+body"
    );
}
