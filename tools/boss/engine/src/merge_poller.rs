//! Periodic PR-lifecycle detection.
//!
//! The on-Stop completion path in [`crate::completion`] handles the
//! create-and-merge case during a run, but most merges happen *after*
//! the worker has exited and released its lease — so no Stop event
//! ever arrives to drive the `in_review → done` transition. Without
//! this module, every chore or project_task that lands its PR after
//! the worker finished would sit in the kanban "Review" column
//! forever waiting for a manual `boss chore update --status done`.
//!
//! The poller also handles the second-most-common in_review fate: the
//! PR develops a merge conflict against its base while waiting for
//! review. The merge-conflict design (`tools/boss/docs/designs/
//! merge-conflict-handling-in-review.md`, Q1) extends `gh pr view`'s
//! projection with `mergeable` / `mergeStateStatus` / `baseRefOid` and
//! flips conflicting parents to `blocked: merge_conflict` so a
//! resolution worker can take over. The same sweep clears that flag
//! when the PR is mergeable again.
//!
//! The poller iterates two candidate lists per sweep:
//!   - [`WorkDb::list_chores_pending_merge_check`] — `in_review` rows
//!     to watch for a clean merge or a fresh conflict.
//!   - [`WorkDb::list_chores_blocked_on_merge_conflict`] — rows the
//!     engine previously flagged as conflicting, to watch for the
//!     resolution signal.
//!
//! Errors are logged but never propagate — a temporary network blip
//! must not crash the engine.
//!
//! `gh pr view` accepts a full PR URL and resolves the repo from the
//! URL itself, so the poller works fine inside the engine's process
//! (no workspace context needed).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use crate::conflict_watch;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::work::{PendingMergeCheck, WorkDb};

/// One slice of GitHub-reported PR lifecycle state, captured by a
/// single `gh pr view` round-trip. Carries everything the poller's
/// sweep dispatch needs to route to merge/conflict/clear paths.
///
/// The "four-state" naming in the design doc refers to the leaf
/// values of [`PrLifecycleState`] — `Open(Clean)`, `Open(Conflict)`,
/// `Merged`, `ClosedUnmerged`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLifecycleProbe {
    pub url: String,
    pub state: PrLifecycleState,
    /// Sha of the PR's base ref at probe time. Captured for the
    /// conflict-resolution flow (`conflict_resolutions.base_sha_at_trigger`,
    /// design Q3); currently informational for the merge poller.
    /// `None` when GitHub didn't report one (rare; usually means the
    /// PR has been force-detached from its base).
    pub base_ref_oid: Option<String>,
    /// Labels currently applied to the PR. Carried so the
    /// conflict-watch / auto-rebase / ci-watch paths can honour the
    /// per-PR opt-out label (`boss/no-auto-rebase`, design Q7 /
    /// Phase 6 #18) without a second `gh` round trip.
    pub labels: Vec<String>,
}

/// Lifecycle states the poller reacts to. The split between
/// `Open(Clean)` and `Open(Conflict)` is the load-bearing addition
/// for the merge-conflict design — they share `state='OPEN'` on the
/// GitHub side and are disambiguated by `mergeable` /
/// `mergeStateStatus`. `Merged` is what the original poller
/// detected. `ClosedUnmerged` is captured for completeness (per the
/// closed-unmerged design); the current sweep treats it as a no-op
/// (a PR force-deleted out of review is the user's problem, not the
/// poller's), preserving prior behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrMergeability),
    Merged,
    ClosedUnmerged,
}

/// Whether an open PR's head ref currently merges cleanly into its
/// base. Derived from GitHub's `mergeable` + `mergeStateStatus`
/// pair. Transient `UNKNOWN` (GitHub is mid-recompute) is mapped to
/// `Clean` per design Q1 — we do not act on UNKNOWN; we wait for
/// definitive `CONFLICTING` on the next sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrMergeability {
    Clean,
    Conflict,
}

/// Probe the lifecycle state of a single PR. Implemented for
/// production by shelling out to `gh`; test doubles can stub it
/// directly.
#[async_trait]
pub trait MergeProbe: Send + Sync {
    /// Returns the latest lifecycle state for `pr_url`. Errors are
    /// reserved for tool / network failures; "PR doesn't exist" is
    /// reported as `Ok` with `state=ClosedUnmerged` so the poller's
    /// in-review-stays-in-review behaviour is preserved (a deleted
    /// PR's row stays where it was).
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe>;
}

/// `MergeProbe` that shells out to `gh pr view <url> --json …`.
#[derive(Debug, Default)]
pub struct CommandMergeProbe;

impl CommandMergeProbe {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MergeProbe for CommandMergeProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,labels",
                "--jq",
                // The labels column is comma-separated so it can ride a
                // single TSV row alongside the scalar fields. `gh`
                // returns a label as `{name,…}`; we project only `.name`
                // and join them. Labels with commas are not supported
                // by GitHub, so the join is unambiguous.
                r#"[
                    (.state // ""),
                    (.mergedAt // ""),
                    (.closedAt // ""),
                    (.mergeable // ""),
                    (.mergeStateStatus // ""),
                    (.baseRefOid // ""),
                    ((.labels // []) | map(.name) | join(","))
                ] | @tsv"#,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to spawn `gh pr view {pr_url}`"))?;
        if !output.status.success() {
            let stderr_lower = String::from_utf8_lossy(&output.stderr).to_lowercase();
            // "could not resolve to a Resource" / 404 means the PR
            // doesn't exist any more (force-deleted, transferred). We
            // can't decide it's merged just because we can't see it,
            // so treat as closed-unmerged (a no-op for the sweep) and
            // leave the chore where it was.
            if stderr_lower.contains("could not resolve")
                || stderr_lower.contains("404")
                || stderr_lower.contains("not found")
            {
                return Ok(PrLifecycleProbe {
                    url: pr_url.to_owned(),
                    state: PrLifecycleState::ClosedUnmerged,
                    base_ref_oid: None,
                    labels: Vec::new(),
                });
            }
            return Err(anyhow!(
                "`gh pr view {pr_url}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        Ok(parse_probe(pr_url, trimmed))
    }
}

/// Map one tab-separated row produced by the `gh pr view --jq` clause
/// into a [`PrLifecycleProbe`]. Pure function so the parsing rules can
/// be unit-tested without shelling out.
fn parse_probe(url: &str, line: &str) -> PrLifecycleProbe {
    let mut parts = line.split('\t');
    let raw_state = parts.next().unwrap_or("").trim();
    let merged_at = parts.next().unwrap_or("").trim();
    let _closed_at = parts.next().unwrap_or("").trim();
    let mergeable = parts.next().unwrap_or("").trim();
    let merge_state_status = parts.next().unwrap_or("").trim();
    let base_ref_oid = parts.next().unwrap_or("").trim();
    let labels_raw = parts.next().unwrap_or("").trim();
    let state = classify_state(raw_state, merged_at, mergeable, merge_state_status);
    let base_ref_oid = if base_ref_oid.is_empty() {
        None
    } else {
        Some(base_ref_oid.to_owned())
    };
    let labels = if labels_raw.is_empty() {
        Vec::new()
    } else {
        labels_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    PrLifecycleProbe {
        url: url.to_owned(),
        state,
        base_ref_oid,
        labels,
    }
}

/// Per-PR opt-out label that suppresses every auto-remediation flow
/// (conflict resolution, auto-rebase, CI fixing) for a single PR.
/// Mirrors the auto-rebase design's Q8 string; this design extends
/// the same label to the conflict-watch path (Q7 / Phase 6 #18).
pub const OPT_OUT_LABEL: &str = "boss/no-auto-rebase";

/// True iff `labels` contains the unified opt-out label
/// ([`OPT_OUT_LABEL`]). Match is case-insensitive — GitHub labels are
/// case-preserving but the engine should tolerate casing drift the
/// user introduces.
pub fn pr_labels_opt_out(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|l| l.eq_ignore_ascii_case(OPT_OUT_LABEL))
}

/// Classification rules (design Q1):
///   - `state=MERGED` or non-empty `mergedAt` → `Merged`.
///   - `state=CLOSED` (and not merged) → `ClosedUnmerged`.
///   - `state=OPEN` (or unknown / empty, treated as still-open):
///       * `mergeable=CONFLICTING` AND `mergeStateStatus=DIRTY` → `Conflict`
///       * everything else (incl. `UNKNOWN`) → `Clean`.
///
/// The two-field agreement on `CONFLICTING` + `DIRTY` is deliberate —
/// either alone is the precise signal, but requiring both protects
/// against `mergeStateStatus` lagging behind `mergeable` immediately
/// after a base move.
fn classify_state(
    raw_state: &str,
    merged_at: &str,
    mergeable: &str,
    merge_state_status: &str,
) -> PrLifecycleState {
    let merged_at_present = !merged_at.is_empty() && !merged_at.eq_ignore_ascii_case("null");
    if raw_state.eq_ignore_ascii_case("MERGED") || merged_at_present {
        return PrLifecycleState::Merged;
    }
    if raw_state.eq_ignore_ascii_case("CLOSED") {
        return PrLifecycleState::ClosedUnmerged;
    }
    let conflicting = mergeable.eq_ignore_ascii_case("CONFLICTING")
        && merge_state_status.eq_ignore_ascii_case("DIRTY");
    if conflicting {
        PrLifecycleState::Open(OpenPrMergeability::Conflict)
    } else {
        PrLifecycleState::Open(OpenPrMergeability::Clean)
    }
}

/// Outcome of one sweep. Used for logging and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepOutcome {
    pub merged: usize,
    pub conflict_flagged: usize,
    pub conflict_cleared: usize,
}

impl SweepOutcome {
    fn total_transitions(self) -> usize {
        self.merged + self.conflict_flagged + self.conflict_cleared
    }
}

/// Run one full lifecycle sweep over every chore and project_task
/// the poller cares about (in_review with a PR, plus rows currently
/// blocked on merge_conflict so we can detect resolution). Returns
/// per-bucket counters so callers can log a one-line summary.
///
/// `cube_client` is threaded into the conflict-watch retire path so
/// `on_resolved` can release the cube workspace lease the resolution
/// worker held (design Q5). Pass `None` for sweeps that don't need to
/// drive lease release — pre-Phase-3 wiring, tests, etc.
pub async fn run_one_pass(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
) -> SweepOutcome {
    let in_review = match work_db.list_chores_pending_merge_check() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list pending merge checks");
            Vec::new()
        }
    };
    let blocked_conflict = match work_db.list_chores_blocked_on_merge_conflict() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list chores blocked on merge_conflict",
            );
            Vec::new()
        }
    };
    let total = in_review.len() + blocked_conflict.len();
    if total == 0 {
        return SweepOutcome::default();
    }
    let mut outcome = SweepOutcome::default();
    for candidate in in_review.iter().chain(blocked_conflict.iter()) {
        sweep_one(work_db, probe, publisher, cube_client, candidate, &mut outcome).await;
    }
    outcome
}

async fn sweep_one(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    let probe_result = match probe.probe(&candidate.pr_url).await {
        Ok(state) => state,
        Err(err) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: probe failed; will retry next pass",
            );
            return;
        }
    };
    match probe_result.state {
        PrLifecycleState::Merged => {
            if mark_merged(work_db, publisher, candidate).await {
                outcome.merged += 1;
            }
        }
        PrLifecycleState::Open(OpenPrMergeability::Conflict) => {
            if conflict_watch::on_conflict_detected(work_db, publisher, candidate, &probe_result)
                .await
            {
                outcome.conflict_flagged += 1;
            }
        }
        PrLifecycleState::Open(OpenPrMergeability::Clean) => {
            if conflict_watch::on_resolved(
                work_db,
                publisher,
                cube_client,
                candidate,
                &probe_result.labels,
            )
            .await
            {
                outcome.conflict_cleared += 1;
            }
        }
        PrLifecycleState::ClosedUnmerged => {
            // Out-of-scope for this design — `chore-lifecycle-pr-closed-unmerged.md`
            // owns the close-unmerged transition. The current sweep
            // leaves the chore where it was, matching the prior
            // poller's behaviour for a PR that has vanished.
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: PR closed without merge; leaving row in place",
            );
        }
    }
}

async fn mark_merged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) -> bool {
    let updated = match work_db.mark_chore_pr_merged(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(task)) => task,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to mark work item merged",
            );
            return false;
        }
    };
    publisher
        .publish_work_item_changed(&candidate.product_id, &updated.id, "pr_merged")
        .await;
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        "merge poller: PR merged; work item moved to done",
    );
    true
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at
/// `interval`. The returned `JoinHandle` is detached by callers —
/// the poller has no shutdown path; aborting the engine process is
/// the only way out, which matches every other engine background
/// task.
///
/// Startup sweep (`chore-lifecycle-pr-closed-unmerged.md` Q9 /
/// `merge-conflict-handling-in-review.md` Phase 6 #17): the first
/// `run_one_pass` fires immediately on spawn so any chore whose PR
/// merged or developed a conflict while the engine was offline gets
/// reconciled on boot. The sweep runs inside the spawned task so
/// engine startup isn't blocked on `gh`; subsequent passes are
/// gated behind `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    probe: Arc<dyn MergeProbe>,
    publisher: Arc<dyn ExecutionPublisher>,
    cube_client: Arc<dyn CubeClient>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                probe.as_ref(),
                publisher.as_ref(),
                Some(cube_client.as_ref()),
            )
            .await;
            if outcome.total_transitions() > 0 {
                tracing::info!(
                    merged = outcome.merged,
                    conflict_flagged = outcome.conflict_flagged,
                    conflict_cleared = outcome.conflict_cleared,
                    "merge poller: sweep transitions",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::ExecutionPublisher;
    use crate::work::{
        CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, WorkDb,
        WorkItem, WorkItemPatch,
    };

    struct StubProbe {
        states: std::sync::Mutex<std::collections::HashMap<String, Result<PrLifecycleProbe, String>>>,
    }

    impl StubProbe {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                states: std::sync::Mutex::new(Default::default()),
            })
        }

        fn set(&self, url: &str, state: PrLifecycleState) {
            self.set_with_base(url, state, None);
        }

        fn set_with_base(&self, url: &str, state: PrLifecycleState, base_ref_oid: Option<&str>) {
            self.states.lock().unwrap().insert(
                url.to_owned(),
                Ok(PrLifecycleProbe {
                    url: url.to_owned(),
                    state,
                    base_ref_oid: base_ref_oid.map(str::to_owned),
                    labels: Vec::new(),
                }),
            );
        }

        fn set_with_labels(&self, url: &str, state: PrLifecycleState, labels: &[&str]) {
            self.states.lock().unwrap().insert(
                url.to_owned(),
                Ok(PrLifecycleProbe {
                    url: url.to_owned(),
                    state,
                    base_ref_oid: None,
                    labels: labels.iter().map(|s| (*s).to_owned()).collect(),
                }),
            );
        }

        fn set_err(&self, url: &str, msg: &str) {
            self.states
                .lock()
                .unwrap()
                .insert(url.to_owned(), Err(msg.to_owned()));
        }
    }

    #[async_trait]
    impl MergeProbe for StubProbe {
        async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
            let map = self.states.lock().unwrap();
            match map.get(pr_url) {
                Some(Ok(state)) => Ok(state.clone()),
                Some(Err(msg)) => Err(anyhow!(msg.clone())),
                None => Ok(PrLifecycleProbe {
                    url: pr_url.to_owned(),
                    state: PrLifecycleState::Open(OpenPrMergeability::Clean),
                    base_ref_oid: None,
                    labels: Vec::new(),
                }),
            }
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        work_events: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(
            &self,
            product_id: &str,
            work_item_id: &str,
            reason: &str,
        ) {
            self.work_events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
        async fn publish_frontend_event_on_product(
            &self,
            _product_id: &str,
            _event: boss_protocol::FrontendEvent,
        ) {
        }
    }

    /// Build a `kind = 'project_task'` row in `in_review` with a PR
    /// attached — the post-completion shape that the merge poller
    /// must also sweep, not just `kind = 'chore'`.
    fn make_project_task_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: format!("Product-{name}"),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: format!("Project-{name}"),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: name.into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
            })
            .unwrap();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, task.id)
    }

    fn make_chore_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: format!("Product-{name}"),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: name.into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
            })
            .unwrap();
        // Move chore directly to in_review with a pr_url, mirroring
        // the post-completion state.
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

    #[tokio::test]
    async fn merged_pr_is_promoted_and_publishes_invalidation() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/1";
        let (product_id, chore_id) = make_chore_in_review(&db, "C1", pr);

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Merged);
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.merged, 1);

        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "done");
                assert_eq!(t.pr_url.as_deref(), Some(pr));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let events = publisher.work_events.lock().await.clone();
        assert!(
            events
                .iter()
                .any(|(p, w, r)| p == &product_id && w == &chore_id && r == "pr_merged"),
            "expected pr_merged work-item event, got {events:?}",
        );
    }

    #[tokio::test]
    async fn open_clean_pr_leaves_chore_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/2";
        let (_pid, chore_id) = make_chore_in_review(&db, "C2", pr);

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Clean));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.merged, 0);
        assert_eq!(outcome.conflict_flagged, 0);
        // No `blocked: merge_conflict` row in the corpus, so the clean
        // signal hits nothing on the resolve side either.
        assert_eq!(outcome.conflict_cleared, 0);
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(publisher.work_events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn probe_failure_does_not_crash_or_promote() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr_a = "https://github.com/foo/bar/pull/3";
        let pr_b = "https://github.com/foo/bar/pull/4";
        let (_pa, chore_a) = make_chore_in_review(&db, "Cerr", pr_a);
        let (_pb, chore_b) = make_chore_in_review(&db, "Cok", pr_b);

        let probe = StubProbe::new();
        probe.set_err(pr_a, "auth broken");
        probe.set(pr_b, PrLifecycleState::Merged);
        let publisher = Arc::new(RecordingPublisher::default());

        // The error on pr_a must not prevent pr_b from being promoted.
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.merged, 1);
        match db.get_work_item(&chore_a).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        match db.get_work_item(&chore_b).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merged_pr_promotes_project_task_to_done() {
        // Regression for the bug where the poller's SQL filter only
        // matched `kind = 'chore'`, leaving Performance project_tasks
        // stuck in `in_review` after their PRs landed (2026-05-07).
        // A `kind = 'project_task'` row with a merged PR must be
        // promoted by the same sweep that handles chores.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr_chore = "https://github.com/foo/bar/pull/100";
        let pr_proj = "https://github.com/foo/bar/pull/101";
        let (_pid_c, chore_id) = make_chore_in_review(&db, "Cmix", pr_chore);
        let (project_product_id, project_task_id) =
            make_project_task_in_review(&db, "PTmix", pr_proj);

        let probe = StubProbe::new();
        probe.set(pr_chore, PrLifecycleState::Merged);
        probe.set(pr_proj, PrLifecycleState::Merged);
        let publisher = Arc::new(RecordingPublisher::default());

        // Both kinds are mergeable, so a single sweep should promote
        // both rows — the project_task one being the regression case.
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(
            outcome.merged, 2,
            "merge poller must sweep both chore and project_task rows",
        );

        match db.get_work_item(&project_task_id).unwrap() {
            WorkItem::Task(t) => {
                assert_eq!(t.kind, "project_task");
                assert_eq!(t.status, "done");
                assert_eq!(t.pr_url.as_deref(), Some(pr_proj));
            }
            other => panic!("expected project_task, got {other:?}"),
        }
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore, got {other:?}"),
        }
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events.iter().any(|(p, w, r)| p == &project_product_id
                && w == &project_task_id
                && r == "pr_merged"),
            "expected pr_merged work-item event for project_task, got {work_events:?}",
        );
    }

    #[tokio::test]
    async fn unmerged_project_task_pr_stays_in_review() {
        // The same negative path as `open_clean_pr_leaves_chore_in_review`,
        // but for `kind = 'project_task'`. Guards against a future
        // change that filters back down to chores only.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/200";
        let (_pid, project_task_id) = make_project_task_in_review(&db, "PTopen", pr);

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Clean));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.total_transitions(), 0);
        match db.get_work_item(&project_task_id).unwrap() {
            WorkItem::Task(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected project_task, got {other:?}"),
        }
        assert!(publisher.work_events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn empty_corpus_is_skipped() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        // No chores in review at all → no work, no errors, no events.
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.total_transitions(), 0);
        assert!(publisher.work_events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn sweep_drives_full_conflict_resolve_cycle() {
        // End-to-end through `run_one_pass`: in_review → conflict
        // (probe says Conflict) → resolved (probe flips to Clean on
        // next pass). The poller picks the row up from the
        // in_review slice for the first pass and from the
        // blocked-conflict slice for the second.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/500";
        let (product, chore) = make_chore_in_review(&db, "Ccycle", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1: probe reports Conflict; row flips to blocked.
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict));
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.conflict_flagged, 1);
        assert_eq!(outcome.conflict_cleared, 0);
        assert_eq!(outcome.merged, 0);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "blocked");
                assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Pass 2 with no change: idempotent — probe still reports
        // Conflict, but row is already blocked, so the
        // mark-conflict UPDATE matches zero rows.
        let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome2.total_transitions(), 0);

        // Pass 3: probe flips to Clean; the blocked-conflict slice
        // picks the row up and clears it back to in_review.
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Clean));
        let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome3.conflict_cleared, 1);
        assert_eq!(outcome3.conflict_flagged, 0);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
                assert!(t.blocked_attempt_id.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Pass 4 with no change: the clear is also idempotent.
        let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome4.total_transitions(), 0);

        // Event trail: blocked → resolved, plus the noop-passes
        // emitted nothing.
        let reasons: Vec<String> = publisher
            .work_events
            .lock()
            .await
            .iter()
            .filter(|(p, w, _)| p == &product && w == &chore)
            .map(|(_, _, r)| r.clone())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "blocked_merge_conflict".to_owned(),
                "merge_conflict_resolved".to_owned(),
            ],
        );
    }

    /// Stub `CubeClient` that records every `release_workspace` call.
    #[derive(Default)]
    struct RecordingCubeClient {
        releases: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeClient for RecordingCubeClient {
        async fn ensure_repo(
            &self,
            _origin: &str,
        ) -> Result<crate::coordinator::CubeRepoHandle> {
            unreachable!("not used in merge_poller tests")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<crate::coordinator::CubeWorkspaceLease> {
            unreachable!("not used in merge_poller tests")
        }
        async fn create_change(
            &self,
            _: &std::path::PathBuf,
            _: &str,
        ) -> Result<crate::coordinator::CubeChangeHandle> {
            unreachable!("not used in merge_poller tests")
        }
        async fn release_workspace(&self, lease_id: &str) -> Result<()> {
            self.releases.lock().await.push(lease_id.to_owned());
            Ok(())
        }
        async fn workspace_status(
            &self,
            _: &std::path::Path,
        ) -> Result<crate::coordinator::CubeWorkspaceStatus> {
            unreachable!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        async fn force_release_lease(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(
            &self,
        ) -> Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn sweep_with_attempt_runs_retire_path_end_to_end() {
        // Phase 4 #10 acceptance: a successful push → next probe →
        // retire path runs end-to-end through `run_one_pass`. The
        // attempt row flips to `succeeded`, the parent goes back to
        // `in_review`, the cube lease is released, and the typed
        // ConflictResolutionSucceeded event lands on the product
        // topic.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/600";
        let (product, chore) = make_chore_in_review(&db, "C-attempt-cycle", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let cube = Arc::new(RecordingCubeClient::default());

        // Pass 1: flip to blocked. Then install the attempt (mirroring
        // Phase 3's worker-spawn path) so the next pass exercises the
        // attempt-aware retire path.
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict));
        run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
        )
        .await;
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 600,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base".into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "lease-600", "ws-600", "worker-600")
            .unwrap();

        // Pass 2: probe flips to Clean. Retire runs.
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Clean));
        let outcome = run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
        )
        .await;
        assert_eq!(outcome.conflict_cleared, 1);

        // Parent in_review with blocked columns cleared.
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
                assert!(t.blocked_attempt_id.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Attempt is succeeded.
        let attempt = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
        assert_eq!(attempt.status, "succeeded");
        assert!(attempt.finished_at.is_some());
        // Lease released exactly once.
        assert_eq!(
            cube.releases.lock().await.as_slice(),
            ["lease-600"],
            "retire path must release the attempt's cube lease through the poller",
        );
    }

    #[tokio::test]
    async fn sweep_promotes_merged_pr_even_when_row_was_blocked() {
        // A blocked-on-conflict row whose PR was force-merged via
        // GitHub's branch-protection override should be promoted by
        // the sweep, not left in `blocked`. The Merged branch of the
        // dispatch runs regardless of which candidate list found the
        // row.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/501";
        let (_product, chore) = make_chore_in_review(&db, "C-force-merged", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // First pass: flip to blocked.
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict));
        run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "blocked"),
            other => panic!("expected chore, got {other:?}"),
        }

        // Second pass: GitHub reports MERGED.
        probe.set(pr, PrLifecycleState::Merged);
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.merged, 1);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "done");
                assert_eq!(t.pr_url.as_deref(), Some(pr));
                assert!(
                    t.blocked_reason.is_none(),
                    "merging out of blocked must clear blocked_reason",
                );
                assert!(t.blocked_attempt_id.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Mapping table for the parser. The truth table here mirrors the
    /// design doc's Q1 classification rules and guards against
    /// future tweaks rewriting them silently.
    #[test]
    fn parse_probe_covers_state_mergeable_status_matrix() {
        struct Case {
            label: &'static str,
            row: &'static str,
            expect: PrLifecycleState,
            expect_base: Option<&'static str>,
        }
        let cases = [
            Case {
                label: "MERGED carries through even if mergeable is empty",
                row: "MERGED\t2026-05-09T12:00:00Z\t\t\t\tabc",
                expect: PrLifecycleState::Merged,
                expect_base: Some("abc"),
            },
            Case {
                label: "non-empty mergedAt overrides state=OPEN (edge: GH lag)",
                row: "OPEN\t2026-05-09T12:00:00Z\t\tMERGEABLE\tCLEAN\tabc",
                expect: PrLifecycleState::Merged,
                expect_base: Some("abc"),
            },
            Case {
                label: "CLOSED without merged falls to ClosedUnmerged",
                row: "CLOSED\t\t2026-05-09T12:00:00Z\t\t\tabc",
                expect: PrLifecycleState::ClosedUnmerged,
                expect_base: Some("abc"),
            },
            Case {
                label: "OPEN + MERGEABLE/CLEAN is Clean",
                row: "OPEN\t\t\tMERGEABLE\tCLEAN\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: Some("abc"),
            },
            Case {
                label: "OPEN + CONFLICTING/DIRTY is Conflict",
                row: "OPEN\t\t\tCONFLICTING\tDIRTY\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Conflict),
                expect_base: Some("abc"),
            },
            Case {
                label: "CONFLICTING without DIRTY status falls to Clean (lag protection)",
                row: "OPEN\t\t\tCONFLICTING\tUNKNOWN\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: Some("abc"),
            },
            Case {
                label: "DIRTY without CONFLICTING falls to Clean (lag protection)",
                row: "OPEN\t\t\tMERGEABLE\tDIRTY\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: Some("abc"),
            },
            Case {
                label: "UNKNOWN mergeable is treated as Clean (transient post-base-move)",
                row: "OPEN\t\t\tUNKNOWN\tUNKNOWN\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: Some("abc"),
            },
            Case {
                label: "BEHIND is mergeable; not a conflict",
                row: "OPEN\t\t\tMERGEABLE\tBEHIND\tabc",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: Some("abc"),
            },
            Case {
                label: "empty base ref is None",
                row: "OPEN\t\t\tMERGEABLE\tCLEAN\t",
                expect: PrLifecycleState::Open(OpenPrMergeability::Clean),
                expect_base: None,
            },
        ];
        for case in cases {
            let probe = parse_probe("https://example.test/pr/1", case.row);
            assert_eq!(
                probe.state, case.expect,
                "case `{}`: state mismatch (row: {:?})",
                case.label, case.row,
            );
            assert_eq!(
                probe.base_ref_oid.as_deref(),
                case.expect_base,
                "case `{}`: base_ref_oid mismatch",
                case.label,
            );
            assert!(
                probe.labels.is_empty(),
                "case `{}`: labels mismatch (no trailing column)",
                case.label,
            );
        }
    }

    /// Labels column rides in the 7th TSV slot. Empty stays empty;
    /// commas split into individual labels. The conflict-watch opt-out
    /// uses these to honour the per-PR `boss/no-auto-rebase` label.
    #[test]
    fn parse_probe_parses_labels_column() {
        let row = "OPEN\t\t\tMERGEABLE\tCLEAN\tabc\tneeds-review,boss/no-auto-rebase";
        let probe = parse_probe("https://example.test/pr/2", row);
        assert_eq!(
            probe.labels,
            vec!["needs-review".to_owned(), "boss/no-auto-rebase".to_owned()],
        );

        let row_empty = "OPEN\t\t\tMERGEABLE\tCLEAN\tabc\t";
        let probe_empty = parse_probe("https://example.test/pr/3", row_empty);
        assert!(probe_empty.labels.is_empty());
    }

    #[test]
    fn pr_labels_opt_out_recognises_label_regardless_of_case() {
        assert!(super::pr_labels_opt_out(&["boss/no-auto-rebase".into()]));
        assert!(super::pr_labels_opt_out(&["Boss/No-Auto-Rebase".into()]));
        assert!(super::pr_labels_opt_out(&[
            "needs-review".into(),
            "BOSS/NO-AUTO-REBASE".into(),
        ]));
        assert!(!super::pr_labels_opt_out(&["needs-review".into()]));
        assert!(!super::pr_labels_opt_out(&[]));
    }

    /// Phase 6 #17 acceptance proxy: a chore whose PR became
    /// conflicting while the engine was offline gets reconciled by
    /// the first `run_one_pass` that runs at startup. The poller
    /// already runs `run_one_pass` immediately on spawn (see
    /// `spawn_loop`), so this test exercises the same path the
    /// startup-sweep relies on: a single in-process `run_one_pass`
    /// flips a pre-existing `in_review` row to `blocked: merge_conflict`
    /// without any prior poller activity.
    #[tokio::test]
    async fn startup_sweep_picks_up_offline_conflict_transition() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/800";
        // Seed the chore in `in_review` with a PR, mirroring the
        // post-restart state of a chore whose PR went CONFLICTING
        // while the engine was down.
        let (_product, chore) = make_chore_in_review(&db, "C-offline-conflict", pr);
        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict));
        let publisher = Arc::new(RecordingPublisher::default());

        // No prior probe activity — this is the very first sweep,
        // exactly what runs at engine startup.
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(
            outcome.conflict_flagged, 1,
            "startup sweep must pick up offline conflicts in one pass",
        );

        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "blocked");
                assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn startup_sweep_resolves_offline_clean_transition() {
        // Mirror case: a chore that was `blocked: merge_conflict`
        // before shutdown, whose PR is mergeable again at restart,
        // must retire on the first startup sweep.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/801";
        let (_product, chore) = make_chore_in_review(&db, "C-offline-clean", pr);
        // Put the row into blocked: merge_conflict directly so the
        // startup sweep has to drive the retire path on its first run.
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrMergeability::Clean));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(
            outcome.conflict_cleared, 1,
            "startup sweep must retire offline-resolved conflicts in one pass",
        );
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn opt_out_label_blocks_conflict_flip_through_sweep() {
        // Sweep-level end-to-end for Phase 6 #18: a labelled PR
        // reporting CONFLICTING leaves the chore in `in_review` and
        // records no transition.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/802";
        let (_product, chore) = make_chore_in_review(&db, "C-optout-sweep", pr);
        let probe = StubProbe::new();
        probe.set_with_labels(
            pr,
            PrLifecycleState::Open(OpenPrMergeability::Conflict),
            &["boss/no-auto-rebase"],
        );
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None).await;
        assert_eq!(outcome.conflict_flagged, 0);
        assert_eq!(outcome.total_transitions(), 0);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(publisher.work_events.lock().await.is_empty());
    }
}
