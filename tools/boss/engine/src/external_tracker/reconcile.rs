//! Reconciler core: `run_one_pass` with per-product processing.
//!
//! Implements Design Question 5 ("The Reconciler Loop") from
//! `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`.
//! The spawn loop is a separate chore (T10); this module ships only the
//! per-product reconcile logic.
//!
//! Behavior 5 (close-on-merge wiring per Design Question 8) is included:
//! after Boss-side SQL is committed, the reconciler issues `close_issue`
//! calls for each merged-PR-linked work item whose upstream is still `Open`.
//! Transient failures are logged and retried on the next tick; the retry
//! intent is derived from SQL state (`status='done'`, `pr_url IS NOT NULL`,
//! upstream still Open) so it survives engine crashes without a separate
//! persistence layer.

use std::collections::{HashMap, HashSet};

use boss_protocol::{CreateChoreInput, CREATED_VIA_EXTERNAL_TRACKER_SYNC};
use tracing::{info, warn};

use super::{
    CloseReason, ExternalTracker, TrackerContext, TrackerCredential, TrackerError, TrackerRegistry,
    UpstreamItem, UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
};
use crate::metrics::Registry;
use crate::work::WorkDb;

// ── Metrics ───────────────────────────────────────────────────────────────────

crate::register_counter!(
    FETCH_SUCCEEDED,
    "external_tracker.fetch_succeeded",
    "Upstream fetch calls that completed without error.",
);
crate::register_counter!(
    FETCH_FAILED,
    "external_tracker.fetch_failed",
    "Upstream fetch calls that errored; reconcile is skipped for that product.",
);
crate::register_counter!(
    IMPORTED,
    "external_tracker.imported",
    "New upstream items imported as Boss chores.",
);
crate::register_counter!(
    CLOSED,
    "external_tracker.closed",
    "Boss rows flipped to done because the upstream observed Closed (Behavior 2).",
);
crate::register_counter!(
    PR_ATTACHED,
    "external_tracker.pr_attached",
    "Boss rows that received a pr_url from an upstream PR association (Behavior 4).",
);
crate::register_counter!(
    PR_MERGE_CLOSE_SUCCEEDED,
    "external_tracker.pr_merge_close_succeeded",
    "close_issue calls that succeeded after a linked PR merged (Behavior 5).",
);
crate::register_counter!(
    PR_MERGE_CLOSE_FAILED,
    "external_tracker.pr_merge_close_failed",
    "close_issue calls that failed (transient or permission) after a linked PR merged (Behavior 5).",
);
crate::register_counter!(
    REVERSE_CLOSE_SUCCEEDED,
    "external_tracker.reverse_close_succeeded",
    "close_issue calls that succeeded from the reverse-close path (Behavior 3).",
);
crate::register_counter!(
    REVERSE_CLOSE_FAILED,
    "external_tracker.reverse_close_failed",
    "close_issue calls that failed from the reverse-close path (Behavior 3).",
);
crate::register_counter!(
    UNBOUND,
    "external_tracker.unbound",
    "Work items whose external ref was cleared because the upstream item left project scope.",
);
crate::register_counter!(
    SKIPPED_CLOSED_AT_FIRST_SIGHT,
    "external_tracker.skipped_closed_at_first_sight",
    "Upstream items already Closed at first import; skipped per the bootstrap rule.",
);
crate::register_counter!(
    SKIP_NO_CREDENTIAL,
    "external_tracker.skip_no_credential",
    "Products skipped because credential resolution failed.",
);

/// Register all reconciler metrics with the engine's registry.
/// Must be called from `metrics::init_all`.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&FETCH_SUCCEEDED);
    registry.register_counter(&FETCH_FAILED);
    registry.register_counter(&IMPORTED);
    registry.register_counter(&CLOSED);
    registry.register_counter(&PR_ATTACHED);
    registry.register_counter(&PR_MERGE_CLOSE_SUCCEEDED);
    registry.register_counter(&PR_MERGE_CLOSE_FAILED);
    registry.register_counter(&REVERSE_CLOSE_SUCCEEDED);
    registry.register_counter(&REVERSE_CLOSE_FAILED);
    registry.register_counter(&UNBOUND);
    registry.register_counter(&SKIPPED_CLOSED_AT_FIRST_SIGHT);
    registry.register_counter(&SKIP_NO_CREDENTIAL);
}

// ── Outcome ───────────────────────────────────────────────────────────────────

/// Per-pass aggregate outcome.  Returned by [`run_one_pass`] for the caller
/// (spawn loop, CLI verb) to emit into logs / metrics.
#[derive(Debug, Default, PartialEq)]
pub struct PassOutcome {
    pub products_processed: usize,
    pub products_skipped: usize,
    pub items_imported: usize,
    pub items_closed: usize,
    pub pr_attached: usize,
    /// Behavior 5: close_issue calls that succeeded after a linked PR merged.
    pub close_issue_succeeded: usize,
    /// Behavior 5: close_issue calls that failed after a linked PR merged.
    pub close_issue_failed: usize,
    pub items_unbound: usize,
    /// Behavior 3: close_issue calls that succeeded via reverse-close.
    pub reverse_close_succeeded: usize,
    /// Behavior 3: close_issue calls that failed via reverse-close.
    pub reverse_close_failed: usize,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run one full reconcile pass across every product that has an external
/// tracker binding (`external_tracker_kind IS NOT NULL`).
///
/// Per-product processing is sequential within a pass (intentional: avoids
/// parallel `JoinSet` complexity for the v1 scale of ~10 products).
/// Individual product failures are logged and counted without aborting the
/// pass for other products.
pub async fn run_one_pass(
    work_db: &WorkDb,
    registry: &TrackerRegistry,
    metrics: &Registry,
) -> PassOutcome {
    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "list_products failed; skipping external tracker pass");
            return PassOutcome::default();
        }
    };

    let mut outcome = PassOutcome::default();
    for product in products {
        let (kind, config) = match (product.external_tracker_kind, product.external_tracker_config)
        {
            (Some(k), Some(c)) => (k, c),
            _ => continue,
        };

        let tracker = match registry.get(&kind) {
            Ok(t) => t,
            Err(e) => {
                warn!(product_id = %product.id, %kind, error = %e,
                    "no tracker registered for kind; skipping product");
                outcome.products_skipped += 1;
                continue;
            }
        };

        let ctx = TrackerContext {
            product_id: product.id.clone(),
            config,
            credential: TrackerCredential::ambient(),
        };

        process_product(work_db, &*tracker, &product.id, &ctx, &mut outcome, metrics)
            .await;
        outcome.products_processed += 1;
    }

    outcome
}

// ── Per-product processing ────────────────────────────────────────────────────

/// Which code path queued this close.  Drives metric selection in the close loop.
enum CloseTrigger {
    /// Behavior 5: a linked PR merged upstream.
    PrMerge,
    /// Behavior 3: boss row flipped to `done` without a merged PR (reverse-close).
    ReverseClose,
}

/// Carries intent to call `close_issue` on the upstream tracker after all
/// Boss-side SQL writes are done.
struct CloseCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
    trigger: CloseTrigger,
}

async fn process_product(
    work_db: &WorkDb,
    tracker: &dyn ExternalTracker,
    product_id: &str,
    ctx: &TrackerContext,
    outcome: &mut PassOutcome,
    metrics: &Registry,
) {
    let reverse_close = ctx.config["reverse_close"].as_bool().unwrap_or(false);

    // ── 1. Fetch upstream items ───────────────────────────────────────────────
    let upstream_items = match tracker.fetch_items(ctx).await {
        Ok(items) => {
            FETCH_SUCCEEDED.inc(metrics);
            items
        }
        Err(e) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e, "fetch_items failed; skipping product this tick");
            return;
        }
    };

    // Fast lookup: canonical_id → upstream item.
    let upstream_map: HashMap<&str, &UpstreamItem> = upstream_items
        .iter()
        .map(|item| (item.upstream_ref.canonical_id.as_str(), item))
        .collect();

    // ── 2. Load existing bindings ─────────────────────────────────────────────
    // Includes unbound rows (external_ref_unbound_at IS NOT NULL) so the
    // reconciler can automatically re-bind items that reappear upstream.
    let existing = match work_db.list_external_refs_for_product(product_id) {
        Ok(refs) => refs,
        Err(e) => {
            warn!(product_id, error = %e, "list_external_refs_for_product failed");
            return;
        }
    };

    // Canonical-ids already known in Boss (active OR previously unbound).
    let known_canonical_ids: HashSet<&str> =
        existing.iter().map(|(_, r)| r.canonical_id.as_str()).collect();

    let mut close_candidates: Vec<CloseCandidate> = Vec::new();

    // ── 3. Reconcile each upstream item ───────────────────────────────────────
    for item in &upstream_items {
        let canonical_id = &item.upstream_ref.canonical_id;

        match work_db.find_by_external_ref(&item.upstream_ref.kind, canonical_id) {
            Ok(Some(task)) => {
                reconcile_existing(
                    work_db,
                    &task,
                    item,
                    reverse_close,
                    &mut close_candidates,
                    outcome,
                    metrics,
                );
            }
            Ok(None) => {
                if known_canonical_ids.contains(canonical_id.as_str()) {
                    // Row exists but is unbound — re-bind and reconcile.
                    if let Some((work_item_id, stored_ref)) =
                        existing.iter().find(|(_, r)| r.canonical_id == *canonical_id)
                    {
                        if let Err(e) = work_db.set_external_ref(
                            work_item_id,
                            &item.upstream_ref.kind,
                            &item.upstream_ref.canonical_id,
                            &item.upstream_ref.raw,
                        ) {
                            warn!(
                                work_item_id,
                                canonical_id, error = %e,
                                "re-bind set_external_ref failed"
                            );
                            continue;
                        }
                        // Now the row is active; reconcile normally.
                        match work_db.find_by_external_ref(
                            &stored_ref.kind,
                            &stored_ref.canonical_id,
                        ) {
                            Ok(Some(task)) => {
                                reconcile_existing(
                                    work_db,
                                    &task,
                                    item,
                                    reverse_close,
                                    &mut close_candidates,
                                    outcome,
                                    metrics,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(work_item_id, error = %e, "find_by_external_ref after re-bind failed");
                            }
                        }
                    }
                } else {
                    import_new(work_db, product_id, item, outcome, metrics);
                }
            }
            Err(e) => {
                warn!(canonical_id, error = %e, "find_by_external_ref failed");
            }
        }
    }

    // ── 4. Unbind items removed from the upstream project ────────────────────
    for (work_item_id, stored_ref) in &existing {
        if stored_ref.unbound_at.is_some() {
            continue; // Already unbound; skip.
        }
        if !upstream_map.contains_key(stored_ref.canonical_id.as_str()) {
            match work_db.clear_external_ref(work_item_id) {
                Ok(()) => {
                    UNBOUND.inc(metrics);
                    outcome.items_unbound += 1;
                    info!(
                        work_item_id,
                        canonical_id = %stored_ref.canonical_id,
                        "upstream item no longer in project scope; external ref unbound"
                    );
                }
                Err(e) => {
                    warn!(work_item_id, error = %e, "clear_external_ref failed");
                }
            }
        }
    }

    // ── 5. Issue close calls post-commit (Behavior 5 and Behavior 3) ──────────
    // Cap at 20 per tick to avoid saturating the rate-limit window.
    const CLOSE_BUDGET: usize = 20;
    for candidate in close_candidates.into_iter().take(CLOSE_BUDGET) {
        let is_b3 = matches!(candidate.trigger, CloseTrigger::ReverseClose);
        match tracker
            .close_issue(ctx, &candidate.upstream_ref, CloseReason::Completed)
            .await
        {
            Ok(()) => {
                if is_b3 {
                    REVERSE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.reverse_close_succeeded += 1;
                    info!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        "Behavior 3: upstream issue closed via reverse-close"
                    );
                } else {
                    PR_MERGE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.close_issue_succeeded += 1;
                    info!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        "Behavior 5: upstream issue closed after merged PR"
                    );
                }
            }
            Err(TrackerError::NotFound(_)) => {
                // Issue already closed (404). Treat as success.
                if is_b3 {
                    REVERSE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.reverse_close_succeeded += 1;
                } else {
                    PR_MERGE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.close_issue_succeeded += 1;
                }
            }
            Err(e) => {
                if is_b3 {
                    REVERSE_CLOSE_FAILED.inc(metrics);
                    outcome.reverse_close_failed += 1;
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "Behavior 3: reverse-close failed (transient or permission); will retry next tick"
                    );
                } else {
                    PR_MERGE_CLOSE_FAILED.inc(metrics);
                    outcome.close_issue_failed += 1;
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "Behavior 5: close_issue failed (transient or permission); will retry next tick"
                    );
                }
            }
        }
    }
}

// ── Per-item helpers ──────────────────────────────────────────────────────────

/// Reconcile an existing Boss work item against the current upstream state.
///
/// - **Behavior 4** (PR attach): if `pr_url` is null and upstream has PR
///   associations, write the best URL.
/// - **Behavior 2** (close-mirror): if upstream is `Closed` and boss is not
///   `done`, flip the boss row.
/// - **Behavior 5** (PR-merge close): if upstream is `Open` and either a
///   merged PR is present in the associations, or the boss row is already
///   `done` with a `pr_url` (retry path), queue a `close_issue` call.
/// - **Behavior 3** (reverse-close, opt-in): if upstream is `Open`, boss is
///   `done`, no merged PR drove the transition, and `reverse_close=true` in
///   the product config, queue a `close_issue` call.
/// - Always bumps `external_ref_synced_at`.
fn reconcile_existing(
    work_db: &WorkDb,
    task: &boss_protocol::Task,
    upstream: &UpstreamItem,
    reverse_close: bool,
    close_candidates: &mut Vec<CloseCandidate>,
    outcome: &mut PassOutcome,
    metrics: &Registry,
) {
    let work_item_id = &task.id;

    // Behavior 4: attach a PR URL if the boss row currently has none.
    if task.pr_url.as_deref().unwrap_or("").is_empty() {
        if let Some(best_pr) = pick_best_pr(&upstream.pr_associations) {
            match work_db.reconciler_attach_pr_url(work_item_id, &best_pr.pr_url) {
                Ok(true) => {
                    PR_ATTACHED.inc(metrics);
                    outcome.pr_attached += 1;
                    info!(work_item_id, pr_url = %best_pr.pr_url, "Behavior 4: pr_url attached");
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(work_item_id, error = %e, "reconciler_attach_pr_url failed");
                }
            }
        }
    }

    match &upstream.status {
        UpstreamStatus::Closed { .. } => {
            // Behavior 2: close-mirror — upstream is done, boss must follow.
            if task.status != "done" && task.status != "archived" {
                match work_db.reconciler_close_work_item(work_item_id) {
                    Ok(true) => {
                        CLOSED.inc(metrics);
                        outcome.items_closed += 1;
                        info!(
                            work_item_id,
                            "Behavior 2: close-mirror — upstream Closed → boss done"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(work_item_id, error = %e, "reconciler_close_work_item failed (Behavior 2)");
                    }
                }
            }
        }
        UpstreamStatus::Open => {
            // Behavior 5: close-on-merge.
            let has_merged_pr = upstream.pr_associations.iter().any(|p| p.merged);
            let boss_is_done = task.status == "done" || task.status == "archived";
            let boss_has_pr = !task.pr_url.as_deref().unwrap_or("").is_empty();

            if has_merged_pr && !boss_is_done {
                // Merged PR detected upstream but boss row not yet done → flip it.
                match work_db.reconciler_close_work_item(work_item_id) {
                    Ok(true) => {
                        outcome.items_closed += 1;
                        info!(
                            work_item_id,
                            "Behavior 5: merged PR detected → boss row → done"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(work_item_id, error = %e, "reconciler_close_work_item failed (Behavior 5)");
                    }
                }
            }

            // Queue close_issue for Behavior 5 if:
            //   (a) merged PR detected in upstream associations, OR
            //   (b) boss is already done with a pr_url (retry from prior failed close)
            if has_merged_pr || (boss_is_done && boss_has_pr) {
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::PrMerge,
                });
            } else if reverse_close && boss_is_done {
                // Behavior 3: boss done without a merged PR driving the
                // transition.  Only queue if B5 didn't already claim it
                // (guarded by the `else` branch above).
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::ReverseClose,
                });
            }
        }
    }

    // Bump synced_at every successful reconcile.
    if let Err(e) = work_db.touch_external_ref_synced_at(work_item_id) {
        warn!(work_item_id, error = %e, "touch_external_ref_synced_at failed");
    }
}

/// Import an upstream item that has no Boss mirror yet.
///
/// Skip if the item is already `Closed` at first sight (bootstrap rule from
/// Design Q7: turning on a binding must not flood Boss with historic closed
/// issues).
fn import_new(
    work_db: &WorkDb,
    product_id: &str,
    upstream: &UpstreamItem,
    outcome: &mut PassOutcome,
    metrics: &Registry,
) {
    // Bootstrap rule: skip items that are already closed.
    if matches!(upstream.status, UpstreamStatus::Closed { .. }) {
        SKIPPED_CLOSED_AT_FIRST_SIGHT.inc(metrics);
        info!(
            canonical_id = %upstream.upstream_ref.canonical_id,
            "skipping already-closed upstream item at first import (bootstrap rule)"
        );
        return;
    }

    let description = format!(
        "> Imported from {}\n\n{}",
        upstream.upstream_url, upstream.body
    );

    let input = CreateChoreInput {
        product_id: product_id.to_owned(),
        name: upstream.title.clone(),
        description: Some(description),
        autostart: false,
        priority: None,
        created_via: Some(CREATED_VIA_EXTERNAL_TRACKER_SYNC.to_owned()),
        repo_remote_url: None,
        effort_level: None,
        model_override: None,
        force_duplicate: true,
    };

    let chore = match work_db.create_chore(input) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                canonical_id = %upstream.upstream_ref.canonical_id,
                error = %e,
                "create_chore failed; skipping upstream item"
            );
            return;
        }
    };

    if let Err(e) = work_db.set_external_ref(
        &chore.id,
        &upstream.upstream_ref.kind,
        &upstream.upstream_ref.canonical_id,
        &upstream.upstream_ref.raw,
    ) {
        warn!(work_item_id = %chore.id, error = %e, "set_external_ref failed after import");
        return;
    }

    // Attach a PR URL if one is already associated upstream.
    if let Some(pr) = pick_best_pr(&upstream.pr_associations) {
        if let Err(e) = work_db.reconciler_attach_pr_url(&chore.id, &pr.pr_url) {
            warn!(work_item_id = %chore.id, error = %e, "reconciler_attach_pr_url failed after import");
        }
    }

    if let Err(e) = work_db.touch_external_ref_synced_at(&chore.id) {
        warn!(work_item_id = %chore.id, error = %e, "touch_external_ref_synced_at failed after import");
    }

    IMPORTED.inc(metrics);
    outcome.items_imported += 1;
    info!(
        work_item_id = %chore.id,
        canonical_id = %upstream.upstream_ref.canonical_id,
        "imported new upstream item as Boss chore"
    );
}

/// Pick the best PR to use as the `pr_url`: prefer merged (highest `merged_at`),
/// then fall back to any unmerged PR association.
fn pick_best_pr(associations: &[UpstreamPrAssociation]) -> Option<&UpstreamPrAssociation> {
    let merged = associations
        .iter()
        .filter(|p| p.merged)
        .max_by_key(|p| (p.merged_at.unwrap_or(0), p.pr_url.as_str()));
    if merged.is_some() {
        return merged;
    }
    associations.iter().max_by_key(|p| p.pr_url.as_str())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use boss_protocol::CreateProductInput;
    use serde_json::json;

    use super::*;
    use crate::external_tracker::{
        CloseReason, ExternalTracker, TrackerConfigError, TrackerContext, TrackerError,
        TrackerRegistry, UpstreamItem, UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
    };
    use crate::metrics::Registry;
    use crate::work::WorkDb;

    // ── SpyTracker ────────────────────────────────────────────────────────────

    /// Test double: records `close_issue` calls and returns pre-configured
    /// responses.  `fetch_items` always returns the same list.
    struct SpyTracker {
        items: Vec<UpstreamItem>,
        close_responses: Mutex<VecDeque<crate::external_tracker::Result<()>>>,
        close_calls: Mutex<Vec<String>>,
    }

    impl SpyTracker {
        fn new(items: Vec<UpstreamItem>) -> Arc<Self> {
            Arc::new(Self {
                items,
                close_responses: Mutex::new(VecDeque::new()),
                close_calls: Mutex::new(Vec::new()),
            })
        }

        fn push_ok(self: &Arc<Self>) -> &Arc<Self> {
            self.close_responses.lock().unwrap().push_back(Ok(()));
            self
        }

        fn push_transient(self: &Arc<Self>) -> &Arc<Self> {
            self.close_responses
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::Transient("network error".to_owned())));
            self
        }

        fn close_calls(&self) -> Vec<String> {
            self.close_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ExternalTracker for SpyTracker {
        fn kind(&self) -> &'static str {
            "spy"
        }

        fn validate_config(
            &self,
            _config: &serde_json::Value,
        ) -> std::result::Result<(), TrackerConfigError> {
            Ok(())
        }

        async fn fetch_items(
            &self,
            _ctx: &TrackerContext,
        ) -> crate::external_tracker::Result<Vec<UpstreamItem>> {
            Ok(self.items.clone())
        }

        async fn fetch_item(
            &self,
            _ctx: &TrackerContext,
            ref_: &UpstreamRef,
        ) -> crate::external_tracker::Result<Option<UpstreamItem>> {
            Ok(self.items.iter().find(|i| i.upstream_ref == *ref_).cloned())
        }

        async fn close_issue(
            &self,
            _ctx: &TrackerContext,
            ref_: &UpstreamRef,
            _reason: CloseReason,
        ) -> crate::external_tracker::Result<()> {
            self.close_calls
                .lock()
                .unwrap()
                .push(ref_.canonical_id.clone());
            let next = self.close_responses.lock().unwrap().pop_front();
            next.unwrap_or(Ok(()))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn in_memory_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb")
    }

    fn spy_registry(tracker: Arc<SpyTracker>) -> TrackerRegistry {
        let mut reg = TrackerRegistry::new();
        reg.register(tracker).expect("register spy tracker");
        reg
    }

    fn spy_config() -> serde_json::Value {
        json!({ "kind": "spy" })
    }

    fn spy_config_reverse_close() -> serde_json::Value {
        json!({ "kind": "spy", "reverse_close": true })
    }

    fn upstream_ref(id: u64) -> UpstreamRef {
        UpstreamRef {
            kind: "spy".to_owned(),
            canonical_id: format!("spy#{id}"),
            raw: json!({ "issue_number": id }),
        }
    }

    fn open_item(id: u64, title: &str) -> UpstreamItem {
        UpstreamItem {
            upstream_ref: upstream_ref(id),
            title: title.to_owned(),
            body: format!("Body of issue {id}"),
            status: UpstreamStatus::Open,
            upstream_url: format!("https://example.com/issues/{id}"),
            labels: vec![],
            assignees: vec![],
            pr_associations: vec![],
            updated_at: 0,
        }
    }

    fn closed_item(id: u64) -> UpstreamItem {
        UpstreamItem {
            status: UpstreamStatus::Closed {
                reason: crate::external_tracker::ClosedReason::Completed,
            },
            ..open_item(id, &format!("Closed issue {id}"))
        }
    }

    fn item_with_merged_pr(id: u64, pr_url: &str) -> UpstreamItem {
        UpstreamItem {
            pr_associations: vec![UpstreamPrAssociation {
                pr_url: pr_url.to_owned(),
                merged: true,
                merged_at: Some(1_779_000_000),
            }],
            ..open_item(id, &format!("Issue {id} with merged PR"))
        }
    }

    fn setup_product_with_tracker(db: &WorkDb) -> boss_protocol::Product {
        let product = db
            .create_product(CreateProductInput {
                name: "Test Product".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .expect("create product");
        db.set_product_external_tracker(
            &product.id,
            Some("spy"),
            Some(&spy_config()),
            false,
        )
        .expect("set external tracker");
        product
    }

    fn setup_product_with_reverse_close(db: &WorkDb) -> boss_protocol::Product {
        let product = db
            .create_product(CreateProductInput {
                name: "Reverse Close Product".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .expect("create product");
        db.set_product_external_tracker(
            &product.id,
            Some("spy"),
            Some(&spy_config_reverse_close()),
            false,
        )
        .expect("set external tracker with reverse_close");
        product
    }

    // ── Test: create (new upstream → new boss row) ────────────────────────────

    #[tokio::test]
    async fn create_imports_new_open_upstream_item() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![open_item(1, "My issue")]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.items_imported, 1, "should import one item");
        assert_eq!(outcome.products_processed, 1);

        let task = db
            .find_by_external_ref("spy", "spy#1")
            .expect("query ok")
            .expect("chore should exist");
        assert_eq!(task.status, "todo");
        assert!(task.name.contains("My issue"), "name should come from title");
        assert_eq!(task.product_id, product.id);
        let ext = task.external_ref.expect("external_ref should be set");
        assert_eq!(ext.canonical_id, "spy#1");
        assert!(ext.synced_at.is_some(), "synced_at should be set after import");
    }

    // ── Test: skip already-closed at first sight ──────────────────────────────

    #[tokio::test]
    async fn closed_at_first_sight_is_skipped() {
        let db = in_memory_db();
        setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![closed_item(2)]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.items_imported, 0, "closed item should be skipped");
        let found = db.find_by_external_ref("spy", "spy#2").expect("query ok");
        assert!(found.is_none(), "should not have imported a closed item");
    }

    // ── Test: close-mirror (Behavior 2) ──────────────────────────────────────

    #[tokio::test]
    async fn close_mirror_sets_boss_row_done_when_upstream_closes() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        // Seed a Boss chore bound to upstream#3.
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Open chore".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(
            &chore.id,
            "spy",
            "spy#3",
            &json!({ "issue_number": 3 }),
        )
        .expect("set_external_ref");

        // Upstream now shows issue as closed.
        let tracker = SpyTracker::new(vec![closed_item(3)]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.items_closed, 1);
        let updated = db
            .find_by_external_ref("spy", "spy#3")
            .expect("query ok")
            .expect("chore should still exist");
        assert_eq!(updated.status, "done", "boss row should be done");
    }

    // ── Test: pr-attach (Behavior 4) ─────────────────────────────────────────

    #[tokio::test]
    async fn pr_attach_writes_pr_url_when_upstream_has_pr_association() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore without PR".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(&chore.id, "spy", "spy#4", &json!({ "issue_number": 4 }))
            .expect("set_external_ref");

        let mut item = open_item(4, "Issue with open PR");
        item.pr_associations = vec![UpstreamPrAssociation {
            pr_url: "https://github.com/example/repo/pull/99".to_owned(),
            merged: false,
            merged_at: None,
        }];

        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.pr_attached, 1);
        let updated = db
            .find_by_external_ref("spy", "spy#4")
            .expect("query ok")
            .expect("chore exists");
        assert_eq!(
            updated.pr_url.as_deref(),
            Some("https://github.com/example/repo/pull/99")
        );
    }

    // ── Test: pr-merge-close (Behavior 5) ────────────────────────────────────

    #[tokio::test]
    async fn pr_merge_close_calls_close_issue_on_tracker_and_marks_boss_done() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore with merged PR".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(&chore.id, "spy", "spy#5", &json!({ "issue_number": 5 }))
            .expect("set_external_ref");

        let tracker = SpyTracker::new(vec![item_with_merged_pr(
            5,
            "https://github.com/example/repo/pull/101",
        )]);
        tracker.push_ok();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.close_issue_succeeded, 1, "close_issue should succeed");
        assert_eq!(outcome.items_closed, 1, "boss row should flip to done");

        let updated = db
            .find_by_external_ref("spy", "spy#5")
            .expect("query ok")
            .expect("chore exists");
        assert_eq!(updated.status, "done");

        let calls = tracker.close_calls();
        assert_eq!(calls, vec!["spy#5"], "close_issue should have been called for spy#5");
    }

    // ── Test: unbind (removed from upstream) ─────────────────────────────────

    #[tokio::test]
    async fn unbind_clears_external_ref_when_upstream_item_disappears() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore that will be unbound".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(&chore.id, "spy", "spy#6", &json!({ "issue_number": 6 }))
            .expect("set_external_ref");

        // Empty upstream: item #6 has been removed from the project.
        let tracker = SpyTracker::new(vec![]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.items_unbound, 1, "one item should be unbound");

        // The row should still exist but the ref should be unbound.
        let refs = db
            .list_external_refs_for_product(&product.id)
            .expect("list ok");
        let (_, stored) = refs.iter().find(|(_, r)| r.canonical_id == "spy#6")
            .expect("stored ref should still exist");
        assert!(stored.unbound_at.is_some(), "unbound_at should be set");
        assert!(stored.synced_at.is_none(), "synced_at should be cleared");
    }

    // ── Test: transient close failure → retry on next tick ───────────────────

    #[tokio::test]
    async fn transient_close_failure_is_retried_on_next_tick() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore for retry test".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(&chore.id, "spy", "spy#7", &json!({ "issue_number": 7 }))
            .expect("set_external_ref");

        let upstream = item_with_merged_pr(7, "https://github.com/example/repo/pull/200");
        let tracker = SpyTracker::new(vec![upstream]);

        // Tick 1: close_issue fails transiently.
        tracker.push_transient();

        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome1 = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome1.close_issue_failed, 1, "tick 1: should record failed close");
        assert_eq!(outcome1.items_closed, 1, "tick 1: boss row should flip to done");

        let after_tick1 = db
            .find_by_external_ref("spy", "spy#7")
            .expect("query ok")
            .expect("chore exists");
        assert_eq!(after_tick1.status, "done", "boss row done after tick 1");

        // Tick 2: upstream is still Open (close didn't land); close_issue succeeds.
        tracker.push_ok();

        let outcome2 = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome2.close_issue_succeeded, 1, "tick 2: close should succeed");
        assert_eq!(outcome2.items_closed, 0, "tick 2: boss already done, no extra close");

        let calls = tracker.close_calls();
        // close_issue called once on tick 1 (failed) and once on tick 2 (succeeded).
        assert_eq!(calls, vec!["spy#7", "spy#7"], "close_issue called on both ticks");
    }

    // ── Test: idempotency — no changes when state is already consistent ───────

    #[tokio::test]
    async fn idempotent_when_already_synced() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let tracker = SpyTracker::new(vec![open_item(8, "Stable issue")]);
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        // First pass: import.
        let outcome1 = run_one_pass(&db, &registry, &metrics).await;
        assert_eq!(outcome1.items_imported, 1);

        // Second pass: nothing should change.
        let outcome2 = run_one_pass(&db, &registry, &metrics).await;
        assert_eq!(outcome2.items_imported, 0);
        assert_eq!(outcome2.items_closed, 0);
        assert_eq!(outcome2.pr_attached, 0);
        assert_eq!(outcome2.close_issue_succeeded, 0);

        // Boss row unchanged.
        let task = db
            .find_by_external_ref("spy", "spy#8")
            .expect("query ok")
            .expect("chore exists");
        assert_eq!(task.status, "todo");
        assert!(task.pr_url.is_none());
        assert_eq!(task.product_id, product.id);
    }

    // ── Test: rebind when upstream item reappears after unbind ───────────────

    #[tokio::test]
    async fn rebind_when_upstream_item_reappears() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        // Seed a chore with external ref bound.
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Rebind test chore".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(&chore.id, "spy", "spy#9", &json!({ "issue_number": 9 }))
            .expect("set_external_ref");

        // Pass 1: upstream is empty → unbind.
        let tracker_empty = SpyTracker::new(vec![]);
        let registry_empty = spy_registry(tracker_empty);
        let metrics = Registry::new();
        register_metrics(&metrics);
        let outcome_unbind = run_one_pass(&db, &registry_empty, &metrics).await;
        assert_eq!(outcome_unbind.items_unbound, 1);

        // Verify unbound.
        let refs = db
            .list_external_refs_for_product(&product.id)
            .expect("list ok");
        let (_, stored) = refs.iter().find(|(_, r)| r.canonical_id == "spy#9").unwrap();
        assert!(stored.unbound_at.is_some(), "should be unbound");

        // Pass 2: item reappears upstream → should re-bind, not create a duplicate.
        let tracker_reappear = SpyTracker::new(vec![open_item(9, "Reappeared issue")]);
        let registry_reappear = spy_registry(tracker_reappear);
        let metrics2 = Registry::new();
        register_metrics(&metrics2);
        let outcome_rebind = run_one_pass(&db, &registry_reappear, &metrics2).await;
        assert_eq!(outcome_rebind.items_imported, 0, "should rebind, not import");

        // Only one chore with spy#9 should exist.
        let rows = db.list_external_refs_for_product(&product.id).expect("list ok");
        let spy9_rows: Vec<_> = rows.iter().filter(|(_, r)| r.canonical_id == "spy#9").collect();
        assert_eq!(spy9_rows.len(), 1, "exactly one binding for spy#9");
        let (_, bound) = spy9_rows[0];
        assert!(bound.unbound_at.is_none(), "should be rebound (unbound_at cleared)");
    }

    // ── Behavior 3 (reverse-close) tests ─────────────────────────────────────

    // Helper: create a chore that is already in `done` status.
    fn seed_done_chore(db: &WorkDb, product_id: &str, canonical_id: &str, issue_num: u64) -> boss_protocol::Task {
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: format!("Done chore {canonical_id}"),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .expect("create chore");
        db.set_external_ref(
            &chore.id,
            "spy",
            canonical_id,
            &json!({ "issue_number": issue_num }),
        )
        .expect("set_external_ref");
        db.reconciler_close_work_item(&chore.id).expect("close work item");
        db.find_by_external_ref("spy", canonical_id)
            .expect("query ok")
            .expect("chore exists")
    }

    /// Behavior 3 happy path: `reverse_close=true`, boss is done, no merged PR
    /// → `close_issue` fires.
    #[tokio::test]
    async fn reverse_close_fires_when_boss_done_and_no_merged_pr() {
        let db = in_memory_db();
        let product = setup_product_with_reverse_close(&db);

        let chore = seed_done_chore(&db, &product.id, "spy#10", 10);
        assert_eq!(chore.status, "done");

        // Upstream still Open, no PR associations.
        let tracker = SpyTracker::new(vec![open_item(10, "Issue 10")]);
        tracker.push_ok();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.reverse_close_succeeded, 1, "reverse-close should succeed");
        assert_eq!(outcome.close_issue_succeeded, 0, "Behavior 5 should NOT fire");

        let calls = tracker.close_calls();
        assert_eq!(calls, vec!["spy#10"], "close_issue should be called once");
    }

    /// Behavior 3 + Behavior 5 mutual exclusion: `reverse_close=true` AND
    /// upstream shows a merged PR → Behavior 5 fires; reverse-close is skipped.
    #[tokio::test]
    async fn reverse_close_skipped_when_pr_merge_drove_transition() {
        let db = in_memory_db();
        let product = setup_product_with_reverse_close(&db);

        // Boss row bound and done, upstream shows a merged PR.
        let chore = seed_done_chore(&db, &product.id, "spy#11", 11);
        assert_eq!(chore.status, "done");

        let tracker = SpyTracker::new(vec![item_with_merged_pr(
            11,
            "https://github.com/example/repo/pull/200",
        )]);
        tracker.push_ok();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        // Behavior 5 fires; reverse-close path must not.
        assert_eq!(outcome.close_issue_succeeded, 1, "Behavior 5 should succeed");
        assert_eq!(outcome.reverse_close_succeeded, 0, "Behavior 3 must not fire");
        assert_eq!(outcome.reverse_close_failed, 0);

        let calls = tracker.close_calls();
        assert_eq!(calls.len(), 1, "close_issue called exactly once");
        assert_eq!(calls[0], "spy#11");
    }

    /// With `reverse_close=false` (the default), boss done without a merged PR
    /// → no `close_issue` call regardless.
    #[tokio::test]
    async fn reverse_close_disabled_by_default_no_close_issue() {
        let db = in_memory_db();
        // Default product has reverse_close absent (defaults to false).
        let product = setup_product_with_tracker(&db);

        let chore = seed_done_chore(&db, &product.id, "spy#12", 12);
        assert_eq!(chore.status, "done");

        // Upstream still Open, no PR associations.
        let tracker = SpyTracker::new(vec![open_item(12, "Issue 12")]);
        // No close response queued — if close_issue is called, it returns Ok(())
        // via the default, but we still assert it wasn't called.
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics).await;

        assert_eq!(outcome.reverse_close_succeeded, 0, "reverse-close disabled");
        assert_eq!(outcome.reverse_close_failed, 0);
        assert_eq!(outcome.close_issue_succeeded, 0, "Behavior 5 should not fire either");

        let calls = tracker.close_calls();
        assert!(calls.is_empty(), "close_issue must not be called when reverse_close=false");
    }
}
