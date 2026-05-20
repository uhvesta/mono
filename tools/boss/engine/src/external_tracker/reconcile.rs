//! Reconciler core: `run_one_pass`, `spawn_loop`, and per-product processing.
//!
//! Implements Design Question 5 ("The Reconciler Loop") from
//! `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`.
//!
//! Behavior 5 (close-on-merge wiring per Design Question 8) is included:
//! after Boss-side SQL is committed, the reconciler issues `close_issue`
//! calls for each merged-PR-linked work item whose upstream is still `Open`.
//! Transient failures are logged and retried on the next tick; the retry
//! intent is derived from SQL state (`status='done'`, `pr_url IS NOT NULL`,
//! upstream still Open) so it survives engine crashes without a separate
//! persistence layer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boss_protocol::{CreateChoreInput, CREATED_VIA_EXTERNAL_TRACKER_SYNC};
use tracing::{info, warn};

use super::{
    CloseReason, ExternalTracker, TrackerContext, TrackerCredential, TrackerError, TrackerRegistry,
    UpstreamItem, UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
};
use crate::metrics::Registry;
use crate::work::WorkDb;

// ── Work-invalidation publisher ───────────────────────────────────────────────

/// Sink for work-invalidation broadcasts emitted by the reconciler.
///
/// Implemented by `ServerState` in production so live UI clients see
/// reconciler-driven mutations (import, close-mirror, PR-attach, unbind)
/// without waiting for a restart or product re-open.
/// `NoopWorkInvalidationPublisher` is used in tests and CLI single-pass paths.
#[async_trait]
pub trait WorkInvalidationPublisher: Send + Sync {
    async fn publish_work_item_invalidated(
        &self,
        product_id: &str,
        work_item_id: &str,
        reason: &str,
    );
}

/// No-op implementation; used in tests and CLI paths where live UI
/// broadcast is not needed.
#[derive(Default)]
pub struct NoopWorkInvalidationPublisher;

#[async_trait]
impl WorkInvalidationPublisher for NoopWorkInvalidationPublisher {
    async fn publish_work_item_invalidated(&self, _: &str, _: &str, _: &str) {}
}

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
crate::register_counter!(
    IN_PROGRESS_SET_SUCCEEDED,
    "external_tracker.in_progress_set_succeeded",
    "set_project_status calls that succeeded when a task moved to active (Behavior 6).",
);
crate::register_counter!(
    IN_PROGRESS_SET_FAILED,
    "external_tracker.in_progress_set_failed",
    "set_project_status calls that failed when a task moved to active (Behavior 6).",
);
crate::register_counter!(
    TRACKED_LABEL_ATTACH_SUCCEEDED,
    "external_tracker.tracked_label_attach_succeeded",
    "add_label calls that succeeded when a fresh upstream item was imported.",
);
crate::register_counter!(
    TRACKED_LABEL_ATTACH_FAILED,
    "external_tracker.tracked_label_attach_failed",
    "add_label calls that failed when a fresh upstream item was imported.",
);

/// Label that the reconciler attaches to upstream items it has imported,
/// so users browsing the upstream tracker can see which issues Boss mirrors.
const TRACKED_LABEL: &str = "tracked";

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
    registry.register_counter(&IN_PROGRESS_SET_SUCCEEDED);
    registry.register_counter(&IN_PROGRESS_SET_FAILED);
    registry.register_counter(&TRACKED_LABEL_ATTACH_SUCCEEDED);
    registry.register_counter(&TRACKED_LABEL_ATTACH_FAILED);
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
    /// Behavior 6: set_project_status calls that succeeded when a task moved to active.
    pub in_progress_set_succeeded: usize,
    /// Behavior 6: set_project_status calls that failed when a task moved to active.
    pub in_progress_set_failed: usize,
    /// Behavior 7: tracked-label add_label calls that succeeded on import.
    pub tracked_label_attach_succeeded: usize,
    /// Behavior 7: tracked-label add_label calls that failed on import.
    pub tracked_label_attach_failed: usize,
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
    publisher: &dyn WorkInvalidationPublisher,
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

        process_product(work_db, &*tracker, &product.id, &ctx, &mut outcome, metrics, publisher)
            .await;
        outcome.products_processed += 1;
    }

    outcome
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
///
/// Fires immediately on spawn (mirrors `dep_unblock_sweep::spawn_loop`
/// and `merge_poller::spawn_loop`) so any stale upstream state is caught
/// at engine startup without waiting for the first interval to elapse.
///
/// Errors per product are logged and counted but never propagate — a
/// transient network blip must not crash the engine.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    registry: Arc<TrackerRegistry>,
    interval: Duration,
    metrics: Arc<Registry>,
    publisher: Arc<dyn WorkInvalidationPublisher>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                registry.as_ref(),
                metrics.as_ref(),
                publisher.as_ref(),
            )
            .await;
            if outcome.products_processed > 0
                || outcome.products_skipped > 0
                || outcome.items_imported > 0
                || outcome.items_closed > 0
                || outcome.pr_attached > 0
                || outcome.close_issue_succeeded > 0
                || outcome.close_issue_failed > 0
                || outcome.items_unbound > 0
                || outcome.in_progress_set_succeeded > 0
                || outcome.in_progress_set_failed > 0
                || outcome.tracked_label_attach_succeeded > 0
                || outcome.tracked_label_attach_failed > 0
            {
                tracing::info!(
                    products_processed = outcome.products_processed,
                    products_skipped = outcome.products_skipped,
                    items_imported = outcome.items_imported,
                    items_closed = outcome.items_closed,
                    pr_attached = outcome.pr_attached,
                    close_issue_succeeded = outcome.close_issue_succeeded,
                    close_issue_failed = outcome.close_issue_failed,
                    items_unbound = outcome.items_unbound,
                    in_progress_set_succeeded = outcome.in_progress_set_succeeded,
                    in_progress_set_failed = outcome.in_progress_set_failed,
                    tracked_label_attach_succeeded = outcome.tracked_label_attach_succeeded,
                    tracked_label_attach_failed = outcome.tracked_label_attach_failed,
                    "external tracker reconciler: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single reconcile pass for one named product.
///
/// Used by the `boss product sync-external-tracker` CLI verb. Returns the
/// pass outcome for the caller to log; returns `None` if the product has no
/// external tracker binding or is not found.
pub async fn run_one_pass_for_product(
    work_db: &WorkDb,
    registry: &TrackerRegistry,
    metrics: &Registry,
    product_id: &str,
    publisher: &dyn WorkInvalidationPublisher,
) -> Option<PassOutcome> {
    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "list_products failed");
            return None;
        }
    };

    let product = products.into_iter().find(|p| p.id == product_id)?;

    let (kind, config) = match (product.external_tracker_kind, product.external_tracker_config) {
        (Some(k), Some(c)) => (k, c),
        _ => return None,
    };

    let tracker = match registry.get(&kind) {
        Ok(t) => t,
        Err(e) => {
            warn!(product_id, %kind, error = %e, "no tracker registered for kind");
            return None;
        }
    };

    let ctx = TrackerContext {
        product_id: product_id.to_owned(),
        config,
        credential: TrackerCredential::ambient(),
    };

    let mut outcome = PassOutcome::default();
    process_product(work_db, &*tracker, product_id, &ctx, &mut outcome, metrics, publisher).await;
    outcome.products_processed += 1;
    Some(outcome)
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
    /// PR URL to reference in the closing comment on the issue, if known.
    pr_url: Option<String>,
}

/// Carries intent to call `set_project_status` (Behavior 6) after all
/// Boss-side SQL writes are done.
struct InProgressCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
}

/// Carries intent to call `add_label` (Behavior 7 retry) for an already-imported
/// item that is missing the `tracked` label upstream.
struct LabelCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
}

async fn process_product(
    work_db: &WorkDb,
    tracker: &dyn ExternalTracker,
    product_id: &str,
    ctx: &TrackerContext,
    outcome: &mut PassOutcome,
    metrics: &Registry,
    publisher: &dyn WorkInvalidationPublisher,
) {
    let reverse_close = ctx.config["reverse_close"].as_bool().unwrap_or(false);
    let in_progress_column =
        ctx.config["in_progress_column"].as_str().unwrap_or("In progress").to_owned();

    // ── 1. Fetch upstream items ───────────────────────────────────────────────
    let upstream_items = match tracker.fetch_items(ctx).await {
        Ok(items) => {
            FETCH_SUCCEEDED.inc(metrics);
            // Clear any stale fetch-failure attention items now that the
            // fetch has succeeded.
            for kind in &[
                "external_tracker_auth_failed",
                "external_tracker_transient_errors",
            ] {
                if let Err(e) = work_db.resolve_external_tracker_attention(product_id, kind) {
                    warn!(product_id, %kind, error = %e, "resolve_external_tracker_attention failed");
                }
            }
            items
        }
        Err(ref e @ TrackerError::Auth(ref msg)) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e, "fetch_items auth failure; skipping product this tick");
            let title = format!("External tracker auth failed for product {product_id}");
            let body = format!(
                "Boss could not authenticate with the external tracker: {msg}\n\n\
                 Run `gh auth login` to refresh credentials, then try again."
            );
            if let Err(attn_err) = work_db.upsert_external_tracker_attention(
                product_id,
                "external_tracker_auth_failed",
                &title,
                &body,
            ) {
                warn!(product_id, error = %attn_err,
                    "upsert_external_tracker_attention (auth_failed) failed");
            }
            return;
        }
        Err(ref e @ TrackerError::Transient(ref msg)) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e,
                "fetch_items transient error; skipping product this tick");
            let title = format!("External tracker fetch failing for product {product_id}");
            let body = format!(
                "Boss is unable to reach the external tracker: {msg}\n\n\
                 This is usually a transient network issue. Boss will retry automatically."
            );
            if let Err(attn_err) = work_db.upsert_external_tracker_attention(
                product_id,
                "external_tracker_transient_errors",
                &title,
                &body,
            ) {
                warn!(product_id, error = %attn_err,
                    "upsert_external_tracker_attention (transient_errors) failed");
            }
            return;
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
    let mut in_progress_candidates: Vec<InProgressCandidate> = Vec::new();
    let mut label_candidates: Vec<LabelCandidate> = Vec::new();

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
                    &in_progress_column,
                    &mut close_candidates,
                    &mut in_progress_candidates,
                    &mut label_candidates,
                    outcome,
                    metrics,
                    product_id,
                    publisher,
                )
                .await;
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
                                    &in_progress_column,
                                    &mut close_candidates,
                                    &mut in_progress_candidates,
                                    &mut label_candidates,
                                    outcome,
                                    metrics,
                                    product_id,
                                    publisher,
                                )
                                .await;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(work_item_id, error = %e, "find_by_external_ref after re-bind failed");
                            }
                        }
                    }
                } else {
                    import_new(
                        work_db, tracker, ctx, product_id, item, outcome, metrics, publisher,
                    )
                    .await;
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
                    publisher
                        .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                        .await;
                    let title = format!(
                        "Upstream binding for {} cleared",
                        stored_ref.canonical_id
                    );
                    let body = format!(
                        "`{}` was bound to upstream `{}` which is no longer in the configured \
                         project. The link has been cleared; re-bind manually with \
                         `boss chore link-external` if this was unintended.",
                        work_item_id, stored_ref.canonical_id
                    );
                    if let Err(e) = work_db.upsert_external_tracker_attention(
                        work_item_id,
                        "external_tracker_removed_upstream",
                        &title,
                        &body,
                    ) {
                        warn!(work_item_id, error = %e,
                            "upsert_external_tracker_attention (removed_upstream) failed");
                    }
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
                if let Some(ref pr_url) = candidate.pr_url {
                    if let Err(e) = tracker
                        .post_closing_pr_comment(ctx, &candidate.upstream_ref, pr_url)
                        .await
                    {
                        warn!(
                            work_item_id = %candidate.work_item_id,
                            canonical_id = %candidate.upstream_ref.canonical_id,
                            error = %e,
                            "post_closing_pr_comment failed (non-fatal); PR linkage comment will be missing"
                        );
                    }
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
            Err(ref e @ TrackerError::PermissionDenied(ref msg)) => {
                if is_b3 {
                    REVERSE_CLOSE_FAILED.inc(metrics);
                    outcome.reverse_close_failed += 1;
                } else {
                    PR_MERGE_CLOSE_FAILED.inc(metrics);
                    outcome.close_issue_failed += 1;
                }
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "close_issue permission denied; credential lacks write scope"
                );
                let title = format!(
                    "Cannot close upstream issue {} — permission denied",
                    candidate.upstream_ref.canonical_id
                );
                let body = format!(
                    "Boss could not close upstream issue `{}`: {msg}\n\n\
                     The credential lacks `issues:write` scope. \
                     Re-run `gh auth login --scopes repo` to grant write permission, \
                     or close the issue manually.",
                    candidate.upstream_ref.canonical_id
                );
                if let Err(e) = work_db.upsert_external_tracker_attention(
                    &candidate.work_item_id,
                    "external_tracker_permission_denied",
                    &title,
                    &body,
                ) {
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        error = %e,
                        "upsert_external_tracker_attention (permission_denied) failed"
                    );
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
                        "Behavior 3: reverse-close failed (transient); will retry next tick"
                    );
                } else {
                    PR_MERGE_CLOSE_FAILED.inc(metrics);
                    outcome.close_issue_failed += 1;
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "Behavior 5: close_issue failed (transient); will retry next tick"
                    );
                }
            }
        }
    }

    // ── 6. Set project status to "In progress" (Behavior 6) ──────────────────
    // Fires when a Boss task entered the active (Doing) state and the upstream
    // item's project column is not already at the configured in-progress value.
    // Cap at 20 per tick to match the close-candidates budget.
    const IN_PROGRESS_BUDGET: usize = 20;
    for candidate in in_progress_candidates.into_iter().take(IN_PROGRESS_BUDGET) {
        match tracker.set_project_status(ctx, &candidate.upstream_ref).await {
            Ok(()) => {
                IN_PROGRESS_SET_SUCCEEDED.inc(metrics);
                outcome.in_progress_set_succeeded += 1;
                info!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    "Behavior 6: project status set to In progress"
                );
            }
            Err(e) => {
                IN_PROGRESS_SET_FAILED.inc(metrics);
                outcome.in_progress_set_failed += 1;
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "Behavior 6: set_project_status failed (transient); will retry next tick"
                );
            }
        }
    }

    // ── 7. Retroactively attach `tracked` label (Behavior 7 retry) ───────────
    // Items whose initial label-add failed (e.g. due to the T630 string-not-array
    // bug) are re-attempted on every reconcile pass until the label is confirmed
    // present in the upstream fetch. Cap at 20 to match other budgets.
    const LABEL_BUDGET: usize = 20;
    for candidate in label_candidates.into_iter().take(LABEL_BUDGET) {
        match tracker.add_label(ctx, &candidate.upstream_ref, TRACKED_LABEL).await {
            Ok(()) => {
                TRACKED_LABEL_ATTACH_SUCCEEDED.inc(metrics);
                outcome.tracked_label_attach_succeeded += 1;
                info!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    "Behavior 7: tracked label attached (reconcile retry)"
                );
            }
            Err(e) => {
                TRACKED_LABEL_ATTACH_FAILED.inc(metrics);
                outcome.tracked_label_attach_failed += 1;
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "Behavior 7: add_label failed on reconcile retry; will retry next tick"
                );
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
/// - **Behavior 7** (tracked-label retry): if the `tracked` label is absent
///   upstream, queue an `add_label` call so the label converges even when
///   the initial import-time attach failed.
/// - Always bumps `external_ref_synced_at`.
async fn reconcile_existing(
    work_db: &WorkDb,
    task: &boss_protocol::Task,
    upstream: &UpstreamItem,
    reverse_close: bool,
    in_progress_column: &str,
    close_candidates: &mut Vec<CloseCandidate>,
    in_progress_candidates: &mut Vec<InProgressCandidate>,
    label_candidates: &mut Vec<LabelCandidate>,
    outcome: &mut PassOutcome,
    metrics: &Registry,
    product_id: &str,
    publisher: &dyn WorkInvalidationPublisher,
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
                    publisher
                        .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                        .await;
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
                        publisher
                            .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                            .await;
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
                        publisher
                            .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                            .await;
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
                let pr_url = if has_merged_pr {
                    pick_best_pr(&upstream.pr_associations).map(|p| p.pr_url.clone())
                } else {
                    task.pr_url.clone()
                };
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::PrMerge,
                    pr_url,
                });
            } else if reverse_close && boss_is_done {
                // Behavior 3: boss done without a merged PR driving the
                // transition.  Only queue if B5 didn't already claim it
                // (guarded by the `else` branch above).
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::ReverseClose,
                    pr_url: task.pr_url.clone(),
                });
            }

            // Behavior 6: mirror boss→active to the upstream project column.
            // Queue only when the task is active (Doing) and the upstream
            // project status is not already the target column; this prevents
            // a regression if the user has manually advanced the item to a
            // later column while the task is still in progress.
            if task.status == "active" {
                let already_at_target =
                    upstream.project_status.as_deref() == Some(in_progress_column);
                if !already_at_target {
                    in_progress_candidates.push(InProgressCandidate {
                        work_item_id: work_item_id.clone(),
                        upstream_ref: upstream.upstream_ref.clone(),
                    });
                }
            }
        }
    }

    // Behavior 7 (retry): if the upstream item doesn't carry the `tracked`
    // label, queue a label-add so the label converges on the next pass.
    // This catches items whose initial import-time add_label failed.
    if !upstream.labels.iter().any(|l| l == TRACKED_LABEL) {
        label_candidates.push(LabelCandidate {
            work_item_id: work_item_id.clone(),
            upstream_ref: upstream.upstream_ref.clone(),
        });
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
async fn import_new(
    work_db: &WorkDb,
    tracker: &dyn ExternalTracker,
    ctx: &TrackerContext,
    product_id: &str,
    upstream: &UpstreamItem,
    outcome: &mut PassOutcome,
    metrics: &Registry,
    publisher: &dyn WorkInvalidationPublisher,
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

    // Use the atomic import method so the chore row and its external_ref
    // binding are committed together. A plain create_chore + set_external_ref
    // pair leaves a crash window where the chore exists but has no ref,
    // making it invisible to the reconciler and breaking reverse_close.
    let chore = match work_db.import_chore_with_external_ref(
        input,
        &upstream.upstream_ref.kind,
        &upstream.upstream_ref.canonical_id,
        &upstream.upstream_ref.raw,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                canonical_id = %upstream.upstream_ref.canonical_id,
                error = %e,
                "import_chore_with_external_ref failed; skipping upstream item"
            );
            return;
        }
    };

    // Attach a PR URL if one is already associated upstream.
    if let Some(pr) = pick_best_pr(&upstream.pr_associations) {
        if let Err(e) = work_db.reconciler_attach_pr_url(&chore.id, &pr.pr_url) {
            warn!(work_item_id = %chore.id, error = %e, "reconciler_attach_pr_url failed after import");
        }
    }

    publisher
        .publish_work_item_invalidated(product_id, &chore.id, "chore_created")
        .await;

    IMPORTED.inc(metrics);
    outcome.items_imported += 1;
    info!(
        work_item_id = %chore.id,
        canonical_id = %upstream.upstream_ref.canonical_id,
        "imported new upstream item as Boss chore"
    );

    // Behavior 7: attach the `tracked` label so humans browsing the upstream
    // tracker can see which issues Boss is mirroring. Skip the API call when
    // the label is already present; failures are logged but never block import.
    if upstream.labels.iter().any(|l| l == TRACKED_LABEL) {
        return;
    }
    match tracker.add_label(ctx, &upstream.upstream_ref, TRACKED_LABEL).await {
        Ok(()) => {
            TRACKED_LABEL_ATTACH_SUCCEEDED.inc(metrics);
            outcome.tracked_label_attach_succeeded += 1;
            info!(
                work_item_id = %chore.id,
                canonical_id = %upstream.upstream_ref.canonical_id,
                "Behavior 7: tracked label attached to upstream item"
            );
        }
        Err(e) => {
            TRACKED_LABEL_ATTACH_FAILED.inc(metrics);
            outcome.tracked_label_attach_failed += 1;
            warn!(
                work_item_id = %chore.id,
                canonical_id = %upstream.upstream_ref.canonical_id,
                error = %e,
                "Behavior 7: add_label failed; import continues, will retry on next sync of this item only if re-imported"
            );
        }
    }
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

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn noop_pub() -> NoopWorkInvalidationPublisher {
        NoopWorkInvalidationPublisher
    }

    /// Records every `publish_work_item_invalidated` call for assertions.
    #[derive(Default)]
    struct RecordingPublisher {
        calls: Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingPublisher {
        fn recorded(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkInvalidationPublisher for RecordingPublisher {
        async fn publish_work_item_invalidated(
            &self,
            product_id: &str,
            work_item_id: &str,
            reason: &str,
        ) {
            self.calls.lock().unwrap().push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
    }

    // ── SpyTracker ────────────────────────────────────────────────────────────

    /// Test double: records `close_issue` and `set_project_status` calls and
    /// returns pre-configured responses.  `fetch_items` returns the item list
    /// unless a fetch error has been queued via `push_fetch_error`.
    struct SpyTracker {
        items: Vec<UpstreamItem>,
        fetch_errors: Mutex<VecDeque<crate::external_tracker::Result<Vec<UpstreamItem>>>>,
        close_responses: Mutex<VecDeque<crate::external_tracker::Result<()>>>,
        close_calls: Mutex<Vec<String>>,
        set_project_status_responses: Mutex<VecDeque<crate::external_tracker::Result<()>>>,
        set_project_status_calls: Mutex<Vec<String>>,
        add_label_responses: Mutex<VecDeque<crate::external_tracker::Result<()>>>,
        add_label_calls: Mutex<Vec<(String, String)>>,
    }

    impl SpyTracker {
        fn new(items: Vec<UpstreamItem>) -> Arc<Self> {
            Arc::new(Self {
                items,
                fetch_errors: Mutex::new(VecDeque::new()),
                close_responses: Mutex::new(VecDeque::new()),
                close_calls: Mutex::new(Vec::new()),
                set_project_status_responses: Mutex::new(VecDeque::new()),
                set_project_status_calls: Mutex::new(Vec::new()),
                add_label_responses: Mutex::new(VecDeque::new()),
                add_label_calls: Mutex::new(Vec::new()),
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

        fn push_permission_denied(self: &Arc<Self>) -> &Arc<Self> {
            self.close_responses
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::PermissionDenied(
                    "credential lacks issues:write".to_owned(),
                )));
            self
        }

        fn push_fetch_auth_error(self: &Arc<Self>) -> &Arc<Self> {
            self.fetch_errors
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::Auth("token invalid".to_owned())));
            self
        }

        fn push_fetch_transient_error(self: &Arc<Self>) -> &Arc<Self> {
            self.fetch_errors
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::Transient("connection refused".to_owned())));
            self
        }

        fn push_set_project_status_ok(self: &Arc<Self>) -> &Arc<Self> {
            self.set_project_status_responses.lock().unwrap().push_back(Ok(()));
            self
        }

        fn push_set_project_status_transient(self: &Arc<Self>) -> &Arc<Self> {
            self.set_project_status_responses
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::Transient("network error".to_owned())));
            self
        }

        fn close_calls(&self) -> Vec<String> {
            self.close_calls.lock().unwrap().clone()
        }

        fn set_project_status_calls(&self) -> Vec<String> {
            self.set_project_status_calls.lock().unwrap().clone()
        }

        fn push_add_label_transient(self: &Arc<Self>) -> &Arc<Self> {
            self.add_label_responses
                .lock()
                .unwrap()
                .push_back(Err(TrackerError::Transient("network error".to_owned())));
            self
        }

        fn add_label_calls(&self) -> Vec<(String, String)> {
            self.add_label_calls.lock().unwrap().clone()
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
            if let Some(next) = self.fetch_errors.lock().unwrap().pop_front() {
                return next;
            }
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

        async fn set_project_status(
            &self,
            _ctx: &TrackerContext,
            ref_: &UpstreamRef,
        ) -> crate::external_tracker::Result<()> {
            self.set_project_status_calls
                .lock()
                .unwrap()
                .push(ref_.canonical_id.clone());
            let next = self.set_project_status_responses.lock().unwrap().pop_front();
            next.unwrap_or(Ok(()))
        }

        async fn add_label(
            &self,
            _ctx: &TrackerContext,
            ref_: &UpstreamRef,
            label: &str,
        ) -> crate::external_tracker::Result<()> {
            self.add_label_calls
                .lock()
                .unwrap()
                .push((ref_.canonical_id.clone(), label.to_owned()));
            let next = self.add_label_responses.lock().unwrap().pop_front();
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
            project_status: None,
        }
    }

    fn open_item_with_project_status(id: u64, title: &str, project_status: &str) -> UpstreamItem {
        UpstreamItem {
            project_status: Some(project_status.to_owned()),
            ..open_item(id, title)
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
                design_repo: None,
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
                design_repo: None,
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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

    // ── Test: import emits work-invalidation event ────────────────────────────

    #[tokio::test]
    async fn import_emits_chore_created_invalidation() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![open_item(99, "Event test issue")]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);
        let publisher = Arc::new(RecordingPublisher::default());

        run_one_pass(&db, &registry, &metrics, publisher.as_ref()).await;

        let calls = publisher.recorded();
        assert_eq!(calls.len(), 1, "expected exactly one invalidation event");
        let (pid, _wid, reason) = &calls[0];
        assert_eq!(pid, &product.id, "product_id should match");
        assert_eq!(reason, "chore_created", "reason should be chore_created");
    }

    // ── Test: tracked label attach (Behavior 7) ───────────────────────────────

    #[tokio::test]
    async fn import_attaches_tracked_label_to_upstream() {
        let db = in_memory_db();
        setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![open_item(50, "Fresh issue")]);
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.items_imported, 1);
        assert_eq!(outcome.tracked_label_attach_succeeded, 1);
        assert_eq!(outcome.tracked_label_attach_failed, 0);
        let calls = tracker.add_label_calls();
        assert_eq!(
            calls,
            vec![("spy#50".to_owned(), "tracked".to_owned())],
            "add_label should be called once with the tracked label"
        );
    }

    #[tokio::test]
    async fn import_skips_add_label_when_already_present_upstream() {
        let db = in_memory_db();
        setup_product_with_tracker(&db);
        let mut item = open_item(51, "Already labelled issue");
        item.labels.push("tracked".to_owned());
        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.items_imported, 1);
        assert_eq!(
            outcome.tracked_label_attach_succeeded, 0,
            "should not count an attach when label already present"
        );
        assert!(
            tracker.add_label_calls().is_empty(),
            "add_label must not be called when 'tracked' is already in upstream.labels"
        );
    }

    #[tokio::test]
    async fn import_succeeds_even_when_add_label_fails_transiently() {
        let db = in_memory_db();
        setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![open_item(52, "Unlucky issue")]);
        tracker.push_add_label_transient();
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        // Import itself must still succeed.
        assert_eq!(outcome.items_imported, 1);
        assert_eq!(outcome.tracked_label_attach_succeeded, 0);
        assert_eq!(outcome.tracked_label_attach_failed, 1);
        let chore = db
            .find_by_external_ref("spy", "spy#52")
            .expect("query ok")
            .expect("chore should exist despite label failure");
        assert_eq!(chore.status, "todo");
    }

    #[tokio::test]
    async fn add_label_not_called_for_closed_at_first_sight() {
        let db = in_memory_db();
        setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![closed_item(53)]);
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert!(
            tracker.add_label_calls().is_empty(),
            "add_label must not be called when an item is skipped at first sight"
        );
    }

    #[tokio::test]
    async fn reconcile_retries_tracked_label_when_missing_on_existing_item() {
        // Behavior 7 retry: an already-imported item whose upstream fetch shows
        // no `tracked` label must trigger add_label on each reconcile pass until
        // the label is confirmed present. This handles the backfill case where
        // the initial import-time add_label failed.
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already bound, label missing".to_owned(),
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
        db.set_external_ref(&chore.id, "spy", "spy#54", &json!({ "issue_number": 54 }))
            .expect("set_external_ref");

        // open_item has no labels — tracked label is missing upstream.
        let tracker = SpyTracker::new(vec![open_item(54, "Already bound issue")]);
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.tracked_label_attach_succeeded, 1,
            "reconcile should attach tracked label when it is missing");
        assert_eq!(
            tracker.add_label_calls(),
            vec![("spy#54".to_owned(), "tracked".to_owned())],
            "add_label must be called once with (canonical_id, 'tracked')"
        );
    }

    #[tokio::test]
    async fn add_label_not_called_when_already_present_on_existing_item() {
        // If the upstream fetch shows `tracked` is already present, the
        // reconciler must not call add_label again (avoid redundant API calls).
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already labeled".to_owned(),
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
        db.set_external_ref(&chore.id, "spy", "spy#55", &json!({ "issue_number": 55 }))
            .expect("set_external_ref");

        let mut item = open_item(55, "Already labeled issue");
        item.labels.push("tracked".to_owned());
        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(tracker.clone());
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert!(
            tracker.add_label_calls().is_empty(),
            "add_label must not be called when 'tracked' is already confirmed upstream"
        );
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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome1.close_issue_failed, 1, "tick 1: should record failed close");
        assert_eq!(outcome1.items_closed, 1, "tick 1: boss row should flip to done");

        let after_tick1 = db
            .find_by_external_ref("spy", "spy#7")
            .expect("query ok")
            .expect("chore exists");
        assert_eq!(after_tick1.status, "done", "boss row done after tick 1");

        // Tick 2: upstream is still Open (close didn't land); close_issue succeeds.
        tracker.push_ok();

        let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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
        let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        assert_eq!(outcome1.items_imported, 1);

        // Second pass: nothing should change.
        let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
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
        let outcome_unbind = run_one_pass(&db, &registry_empty, &metrics, &noop_pub()).await;
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
        let outcome_rebind = run_one_pass(&db, &registry_reappear, &metrics2, &noop_pub()).await;
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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

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

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.reverse_close_succeeded, 0, "reverse-close disabled");
        assert_eq!(outcome.reverse_close_failed, 0);
        assert_eq!(outcome.close_issue_succeeded, 0, "Behavior 5 should not fire either");

        let calls = tracker.close_calls();
        assert!(calls.is_empty(), "close_issue must not be called when reverse_close=false");
    }

    // ── E2E: import → done → reverse_close ───────────────────────────────────

    /// Verify the full import→done→reverse_close path using `run_one_pass` for
    /// the import (rather than the `seed_done_chore` helper that bypasses
    /// `import_chore_with_external_ref`).  This guards against regressions
    /// where the importer creates the chore and the external_ref binding
    /// non-atomically, leaving the chore invisible to the reconciler.
    #[tokio::test]
    async fn reverse_close_fires_for_reconciler_imported_chore() {
        let db = in_memory_db();
        let _product = setup_product_with_reverse_close(&db);

        // Pass 1: import the upstream item as a new Boss chore.
        let tracker = SpyTracker::new(vec![open_item(30, "Issue 30")]);
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        assert_eq!(outcome1.items_imported, 1, "pass 1: should import one item");

        // The imported chore must have its external_ref bound so the
        // reconciler can reach it on subsequent passes.
        let chore = db
            .find_by_external_ref("spy", "spy#30")
            .expect("query ok")
            .expect("imported chore must be findable by external_ref");
        assert_eq!(chore.status, "todo");

        // Simulate the chore being completed (e.g. PR merged → boss dragged to done).
        db.reconciler_close_work_item(&chore.id).expect("close work item");
        let closed = db
            .find_by_external_ref("spy", "spy#30")
            .expect("query ok")
            .expect("chore still exists");
        assert_eq!(closed.status, "done");

        // Pass 2: upstream still Open, boss is done, no merged PR
        //         → reverse_close must fire.
        tracker.push_ok();
        let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome2.reverse_close_succeeded, 1,
            "reverse_close must fire for an imported-then-done chore");
        assert_eq!(outcome2.items_imported, 0,
            "no duplicate import must occur");

        let calls = tracker.close_calls();
        assert_eq!(calls, vec!["spy#30"], "close_issue called exactly once");
    }

    // ── Smoke test: spawn_loop fires one tick and emits metrics ───────────────
    //
    // This test verifies the `spawn_loop` structural contract:
    //   1. The spawned task runs `run_one_pass` immediately on boot.
    //   2. Metrics are emitted (via the shared `Arc<Registry>`).
    //   3. The interval sleep is honoured (loop does not busy-spin).
    //
    // Implementation note: `spawn_loop` moves the DB Arc into the spawned
    // task. For in-memory SQLite shared-cache databases, every call to
    // `connect()` opens a new connection to the same named in-memory
    // database, so both the test thread and the spawned task see the same
    // rows. The interval is set to 1 hour so only the initial on-boot tick
    // fires during the test.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_loop_fires_one_tick_and_emits_metrics() {
        let db = Arc::new(in_memory_db());
        let product = setup_product_with_tracker_arc(db.as_ref());

        // Verify the setup is visible through a fresh connect() before spawning,
        // so a failure here points to setup rather than the loop.
        let products = db.list_products().expect("list_products");
        let bound = products
            .iter()
            .find(|p| p.id == product.id && p.external_tracker_kind.is_some())
            .expect("product with tracker should be visible");
        assert_eq!(bound.external_tracker_kind.as_deref(), Some("spy"));

        let tracker = SpyTracker::new(vec![open_item(10, "Loop issue")]);
        let registry = Arc::new(spy_registry(tracker));

        let metrics = Arc::new(Registry::new());
        register_metrics(&metrics);

        // Use a large interval: only the immediate first tick fires before abort.
        let interval = std::time::Duration::from_secs(3600);
        let handle = spawn_loop(
            db.clone(),
            registry,
            interval,
            metrics.clone(),
            Arc::new(noop_pub()),
        );

        // Poll until the imported counter advances (max 5 s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let imported = metrics.counter_value("external_tracker.imported").unwrap_or(0);
            if imported >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                handle.abort();
                panic!("spawn_loop did not import any item within 5 seconds");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        handle.abort();

        // The spawned task should have imported the one item.
        let task = db
            .find_by_external_ref("spy", "spy#10")
            .expect("query ok")
            .expect("chore should exist after spawn_loop tick");
        assert_eq!(task.status, "todo");

        let imported = metrics.counter_value("external_tracker.imported").unwrap_or(0);
        assert!(imported >= 1, "IMPORTED counter should be ≥ 1, got {imported}");
    }

    fn setup_product_with_tracker_arc(db: &WorkDb) -> boss_protocol::Product {
        setup_product_with_tracker(db)
    }

    // ── Smoke test: run_one_pass_for_product ─────────────────────────────────

    #[tokio::test]
    async fn run_one_pass_for_product_returns_outcome_for_bound_product() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![open_item(11, "Single product issue")]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass_for_product(&db, &registry, &metrics, &product.id, &noop_pub())
            .await
            .expect("should return Some for a bound product");

        assert_eq!(outcome.items_imported, 1);
        assert_eq!(outcome.products_processed, 1);
    }

    #[tokio::test]
    async fn run_one_pass_for_product_returns_none_for_unbound_product() {
        let db = in_memory_db();
        let product = db
            .create_product(boss_protocol::CreateProductInput {
                name: "Unbound".to_owned(),
                description: None,
                repo_remote_url: None,
                design_repo: None,
            })
            .expect("create product");
        let registry = TrackerRegistry::new();
        let metrics = Registry::new();
        register_metrics(&metrics);

        let result =
            run_one_pass_for_product(&db, &registry, &metrics, &product.id, &noop_pub()).await;
        assert!(result.is_none(), "unbound product should return None");
    }

    // ── Attention item integration tests (chore 16) ───────────────────────────

    fn attention_items_for_product(db: &WorkDb, product_id: &str) -> Vec<boss_protocol::WorkAttentionItem> {
        db.list_attention_items_for_work_item(product_id).expect("list attention items")
    }

    fn attention_items_for_work_item(db: &WorkDb, work_item_id: &str) -> Vec<boss_protocol::WorkAttentionItem> {
        db.list_attention_items_for_work_item(work_item_id).expect("list attention items")
    }

    /// Reason 1: auth failure on `fetch_items` emits `external_tracker_auth_failed`
    /// on the product.
    #[tokio::test]
    async fn attention_item_emitted_for_auth_failure() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![]);
        tracker.push_fetch_auth_error();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        let items = attention_items_for_product(&db, &product.id);
        let auth_items: Vec<_> = items.iter()
            .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
            .collect();
        assert_eq!(auth_items.len(), 1, "should emit exactly one auth_failed attention item");
        assert!(
            auth_items[0].body_markdown.contains("gh auth login"),
            "body should contain remediation hint; got: {}",
            auth_items[0].body_markdown
        );
    }

    /// Reason 2: transient fetch error emits `external_tracker_transient_errors`
    /// on the product.
    #[tokio::test]
    async fn attention_item_emitted_for_transient_fetch_error() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![]);
        tracker.push_fetch_transient_error();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        let items = attention_items_for_product(&db, &product.id);
        let transient_items: Vec<_> = items.iter()
            .filter(|i| i.kind == "external_tracker_transient_errors" && i.status == "open")
            .collect();
        assert_eq!(transient_items.len(), 1,
            "should emit exactly one transient_errors attention item");
    }

    /// Reason 3: upstream item removed from project emits
    /// `external_tracker_removed_upstream` on the unbound work item.
    #[tokio::test]
    async fn attention_item_emitted_for_removed_upstream() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore to be unbound".to_owned(),
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
        db.set_external_ref(&chore.id, "spy", "spy#20", &json!({ "issue_number": 20 }))
            .expect("set_external_ref");

        // Empty upstream: spy#20 is no longer in scope.
        let tracker = SpyTracker::new(vec![]);
        let registry = spy_registry(tracker);
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        let items = attention_items_for_work_item(&db, &chore.id);
        let unbound_items: Vec<_> = items.iter()
            .filter(|i| i.kind == "external_tracker_removed_upstream" && i.status == "open")
            .collect();
        assert_eq!(unbound_items.len(), 1,
            "should emit exactly one removed_upstream attention item on the work item");
        assert!(
            unbound_items[0].body_markdown.contains("spy#20"),
            "body should reference the canonical_id; got: {}",
            unbound_items[0].body_markdown
        );
    }

    /// Reason 4: `close_issue` permission denied emits
    /// `external_tracker_permission_denied` on the work item.
    #[tokio::test]
    async fn attention_item_emitted_for_permission_denied_on_close() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Chore with permission-denied close".to_owned(),
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
        db.set_external_ref(&chore.id, "spy", "spy#21", &json!({ "issue_number": 21 }))
            .expect("set_external_ref");

        // Upstream shows a merged PR; boss row is not yet done → close_issue fires.
        let tracker = SpyTracker::new(vec![item_with_merged_pr(
            21,
            "https://github.com/example/repo/pull/300",
        )]);
        tracker.push_permission_denied();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        let items = attention_items_for_work_item(&db, &chore.id);
        let perm_items: Vec<_> = items.iter()
            .filter(|i| i.kind == "external_tracker_permission_denied" && i.status == "open")
            .collect();
        assert_eq!(perm_items.len(), 1,
            "should emit exactly one permission_denied attention item");
        assert!(
            perm_items[0].body_markdown.contains("issues:write"),
            "body should mention required scope; got: {}",
            perm_items[0].body_markdown
        );
    }

    /// Idempotency: a second pass with the same auth failure does not create
    /// a duplicate attention item.
    #[tokio::test]
    async fn attention_items_are_idempotent_on_repeated_failures() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![]);
        tracker.push_fetch_auth_error();
        tracker.push_fetch_auth_error();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        let items = attention_items_for_product(&db, &product.id);
        let auth_items: Vec<_> = items.iter()
            .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
            .collect();
        assert_eq!(auth_items.len(), 1,
            "repeated auth failures must not pile up duplicate attention items");
    }

    /// Recovery: a successful fetch clears stale fetch-failure attention items.
    #[tokio::test]
    async fn attention_items_cleared_on_recovery() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);
        let tracker = SpyTracker::new(vec![]);
        tracker.push_fetch_auth_error();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        // Tick 1: auth failure → attention item created.
        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        let items = attention_items_for_product(&db, &product.id);
        assert!(
            items.iter().any(|i| i.kind == "external_tracker_auth_failed" && i.status == "open"),
            "attention item should exist after auth failure"
        );

        // Tick 2: fetch succeeds (no more queued error) → attention item resolved.
        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        let items2 = attention_items_for_product(&db, &product.id);
        let still_open = items2.iter()
            .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
            .count();
        assert_eq!(still_open, 0, "auth_failed attention item should be resolved after recovery");
    }

    // ── Behavior 6: set project status to "In progress" ──────────────────────

    fn seed_active_chore(
        db: &WorkDb,
        product_id: &str,
        canonical_id: &str,
        issue_num: u64,
    ) -> boss_protocol::Task {
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: format!("Active chore {canonical_id}"),
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
        // Simulate the task being dragged to Doing (active) via direct SQL,
        // mirroring what the engine's update_task RPC does.
        let conn = db.connect().expect("connect for seed_active_chore");
        conn.execute(
            "UPDATE tasks SET status = 'active' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .expect("set status to active");
        db.find_by_external_ref("spy", canonical_id)
            .expect("query ok")
            .expect("chore exists")
    }

    /// Behavior 6 happy path: boss is active, upstream is Open, project_status
    /// is "Todo" → set_project_status fires.
    #[tokio::test]
    async fn set_project_status_fires_when_boss_active_and_upstream_open_todo() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = seed_active_chore(&db, &product.id, "spy#30", 30);
        assert_eq!(chore.status, "active");

        // Upstream is Open with project_status = "Todo" (not yet In progress).
        let item = open_item_with_project_status(30, "Issue 30", "Todo");
        let tracker = SpyTracker::new(vec![item]);
        tracker.push_set_project_status_ok();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.in_progress_set_succeeded, 1, "Behavior 6 should succeed");
        assert_eq!(outcome.in_progress_set_failed, 0);

        let calls = tracker.set_project_status_calls();
        assert_eq!(calls, vec!["spy#30"], "set_project_status called for spy#30");
    }

    /// Behavior 6 is idempotent: if the upstream item is already "In progress",
    /// set_project_status must NOT be called.
    #[tokio::test]
    async fn set_project_status_skipped_when_already_in_progress() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = seed_active_chore(&db, &product.id, "spy#31", 31);
        assert_eq!(chore.status, "active");

        // Upstream already at "In progress".
        let item = open_item_with_project_status(31, "Issue 31", "In progress");
        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.in_progress_set_succeeded, 0, "should not fire when already In progress");
        assert_eq!(outcome.in_progress_set_failed, 0);
        assert!(
            tracker.set_project_status_calls().is_empty(),
            "set_project_status must not be called when already at target"
        );
    }

    /// Behavior 6 does not fire when the Boss task is in todo or done.
    #[tokio::test]
    async fn set_project_status_not_fired_for_non_active_task() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        // todo task
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Todo chore".to_owned(),
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
        db.set_external_ref(&chore.id, "spy", "spy#32", &json!({ "issue_number": 32 }))
            .expect("set_external_ref");
        // Leave as todo (default).

        let item = open_item_with_project_status(32, "Issue 32", "Todo");
        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert_eq!(outcome.in_progress_set_succeeded, 0, "should not fire for todo task");
        assert!(tracker.set_project_status_calls().is_empty());
    }

    /// Behavior 6 does not fire when upstream is Closed (Behavior 2 handles that).
    #[tokio::test]
    async fn set_project_status_not_fired_when_upstream_closed() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = seed_active_chore(&db, &product.id, "spy#33", 33);
        assert_eq!(chore.status, "active");

        // Upstream is Closed → Behavior 2 fires; Behavior 6 should not.
        let item = UpstreamItem {
            status: UpstreamStatus::Closed {
                reason: crate::external_tracker::ClosedReason::Completed,
            },
            project_status: Some("Done".to_owned()),
            ..open_item(33, "Closed issue 33")
        };
        let tracker = SpyTracker::new(vec![item]);
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        run_one_pass(&db, &registry, &metrics, &noop_pub()).await;

        assert!(
            tracker.set_project_status_calls().is_empty(),
            "set_project_status must not be called when upstream is Closed"
        );
    }

    /// Behavior 6 transient failure: set_project_status is retried on the next tick.
    #[tokio::test]
    async fn set_project_status_transient_failure_retried_on_next_tick() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = seed_active_chore(&db, &product.id, "spy#34", 34);
        assert_eq!(chore.status, "active");

        // project_status remains "Todo" both ticks (mutation didn't land yet).
        let item = open_item_with_project_status(34, "Issue 34", "Todo");
        let tracker = SpyTracker::new(vec![item]);

        // Tick 1: set_project_status fails transiently.
        tracker.push_set_project_status_transient();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        assert_eq!(outcome1.in_progress_set_failed, 1, "tick 1: should record failure");
        assert_eq!(outcome1.in_progress_set_succeeded, 0);

        // Tick 2: succeeds.
        tracker.push_set_project_status_ok();
        let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        assert_eq!(outcome2.in_progress_set_succeeded, 1, "tick 2: should succeed");

        let calls = tracker.set_project_status_calls();
        assert_eq!(calls, vec!["spy#34", "spy#34"], "called on both ticks");
    }

    /// Behavior 6 fires when project_status is None (item has no Status column set yet).
    #[tokio::test]
    async fn set_project_status_fires_when_project_status_none() {
        let db = in_memory_db();
        let product = setup_product_with_tracker(&db);

        let chore = seed_active_chore(&db, &product.id, "spy#35", 35);
        assert_eq!(chore.status, "active");

        // project_status is None (Status field not set on the GitHub Project item).
        let item = open_item(35, "Issue 35");
        assert!(item.project_status.is_none());
        let tracker = SpyTracker::new(vec![item]);
        tracker.push_set_project_status_ok();
        let registry = spy_registry(Arc::clone(&tracker));
        let metrics = Registry::new();
        register_metrics(&metrics);

        let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub()).await;
        assert_eq!(outcome.in_progress_set_succeeded, 1, "should fire when project_status is None");
        let calls = tracker.set_project_status_calls();
        assert_eq!(calls, vec!["spy#35"]);
    }
}
