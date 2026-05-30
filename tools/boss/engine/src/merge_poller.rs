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
//! The poller iterates candidate lists per sweep:
//!   - [`WorkDb::list_chores_pending_merge_check`] — `in_review` rows
//!     to watch for a clean merge or a fresh conflict.
//!   - [`WorkDb::list_chores_blocked_on_merge_conflict`] — rows the
//!     engine previously flagged as conflicting, to watch for the
//!     resolution signal.
//!   - [`WorkDb::list_stranded_conflict_resolution_attempts`] — rows
//!     whose `conflict_resolutions` attempt is `pending` but has no live
//!     execution. The sweep re-emits a fresh execution request so a
//!     worker can be dispatched (covers engine-restart and worker-die
//!     gaps without a full PR probe).
//!
//! Errors are logged but never propagate — a temporary network blip
//! must not crash the engine.
//!
//! `gh pr view` accepts a full PR URL and resolves the repo from the
//! URL itself, so the poller works fine inside the engine's process
//! (no workspace context needed).

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::ci_watch;
use crate::completion::{StopOutcome, WorkerCompletionHandler};
use crate::conflict_watch;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::design_detector;
use crate::metrics::Registry;
use crate::work::{LatePrCandidate, PendingMergeCheck, WorkDb};
use boss_protocol;

/// Review-gating state of a PR at probe time. Derived from
/// GitHub's `reviewDecision` field and the `reviews` array.
///
/// `Required` maps to `REVIEW_REQUIRED` — at least one approving
/// review is still needed. `Approved` means all required reviewers
/// have approved; the `reviewers` list carries their login names
/// for the tooltip. `ChangesRequested` means at least one reviewer
/// blocked the PR; `reviewers` lists who. `Unknown` is the
/// fallback when GitHub omitted the field or returned an
/// unrecognised value (e.g., no branch protection configured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrReviewState {
    Required,
    Approved { reviewers: Vec<String> },
    ChangesRequested { reviewers: Vec<String> },
    Unknown,
}

impl PrReviewState {
    /// Stable DB string for the `review_required_state` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PrReviewState::Required => "required",
            PrReviewState::Approved { .. } => "approved",
            PrReviewState::ChangesRequested { .. } => "changes_requested",
            PrReviewState::Unknown => "unknown",
        }
    }

    /// Reviewer login names for the tooltip, if available.
    pub fn reviewers(&self) -> &[String] {
        match self {
            PrReviewState::Approved { reviewers } | PrReviewState::ChangesRequested { reviewers } => reviewers,
            _ => &[],
        }
    }
}

crate::register_counter!(
    MERGED,
    "merge_poller.merged",
    "PRs transitioned to merged in one sweep."
);
crate::register_counter!(
    CONFLICT_FLAGGED,
    "merge_poller.conflict_flagged",
    "PRs flipped to blocked:merge_conflict in one sweep."
);
crate::register_counter!(
    CONFLICT_CLEARED,
    "merge_poller.conflict_cleared",
    "PRs cleared from blocked:merge_conflict in one sweep."
);
crate::register_counter!(
    PR_RECHECK_RECOVERED,
    "merge_poller.pr_recheck_recovered",
    "Missed PR-open transitions recovered by recheck in one sweep."
);
crate::register_counter!(
    PR_RECHECK_UNRESOLVED,
    "merge_poller.pr_recheck_unresolved",
    "PR-detection rechecks that still found no bindable PR in one sweep."
);
crate::register_counter!(
    MERGE_QUEUE_REBOUNCED,
    "merge_poller.merge_queue_rebounced",
    "PRs flipped to blocked:ci_failure due to a merge-queue FAILED_CHECKS dequeue in one sweep."
);
crate::register_counter!(
    LATE_PR_RECOVERED,
    "merge_poller.late_pr_recovered",
    "Late PRs bound to active tasks from terminal executions (double-spawn recovery) in one sweep."
);
crate::register_counter!(
    REVISION_INVALIDATED,
    "merge_poller.revision_invalidated",
    "Pending/active revision executions stopped because their parent PR merged or closed in one sweep."
);
crate::register_counter!(
    WORKER_STOPPED_ON_REVIEW,
    "merge_poller.worker_stopped_on_review",
    "Live worker executions stopped because their task auto-transitioned to in_review (CI detected green) in one sweep."
);

/// Register all merge-poller counter handles with `registry`. Called
/// from [`crate::metrics::init_all`] at engine startup.
pub fn init(registry: &Registry) {
    registry.register_counter(&MERGED);
    registry.register_counter(&CONFLICT_FLAGGED);
    registry.register_counter(&CONFLICT_CLEARED);
    registry.register_counter(&PR_RECHECK_RECOVERED);
    registry.register_counter(&PR_RECHECK_UNRESOLVED);
    registry.register_counter(&MERGE_QUEUE_REBOUNCED);
    registry.register_counter(&LATE_PR_RECOVERED);
    registry.register_counter(&REVISION_INVALIDATED);
    registry.register_counter(&WORKER_STOPPED_ON_REVIEW);
}

/// One slice of GitHub-reported PR lifecycle state, captured by a
/// single `gh pr view` round-trip. Carries everything the poller's
/// sweep dispatch needs to route to merge/conflict/CI/clear paths.
///
/// The "four-state" naming in the design doc refers to the leaf
/// values of [`PrLifecycleState`] — `Open(...)` (with its own
/// mergeability + ci sub-state), `Merged`, `ClosedUnmerged`.
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
    /// Sha of the PR's head ref at probe time. The CI-watch path's
    /// idempotency key (`(work_item_id, head_sha_at_trigger,
    /// attempt_kind)`) needs this; `None` when GitHub didn't report
    /// it (rare).
    pub head_ref_oid: Option<String>,
    /// Name of the PR's head branch (e.g. `"my-feature"`). Required by
    /// the conflict-resolution attempt row (`head_branch` column); `None`
    /// when GitHub didn't report it.
    pub head_ref_name: Option<String>,
    /// Name of the PR's base branch (e.g. `"main"`). Required by the
    /// conflict-resolution attempt row (`base_branch` column); `None`
    /// when GitHub didn't report it.
    pub base_ref_name: Option<String>,
    /// Labels currently applied to the PR. Carried so the
    /// conflict-watch / auto-rebase / ci-watch paths can honour the
    /// per-PR opt-out label (`boss/no-auto-rebase`, design Q7 /
    /// Phase 6 #18) without a second `gh` round trip.
    pub labels: Vec<String>,
    /// Review-gating state derived from GitHub's `reviewDecision` and
    /// `reviews` fields. Used by the merge poller to update the
    /// `review_required_state` / `review_required_detail` columns on
    /// the task row for display in the macOS kanban Review-lane card.
    pub review: PrReviewState,
    /// Whether the PR is currently in GitHub's merge queue at probe time.
    /// Derived from `mergeQueueEntry` — non-null means in queue, null means
    /// not queued. Used to render the merging indicator on Review-lane cards
    /// (replaces the CI icon while the PR is merging).
    pub in_merge_queue: bool,
}

/// Lifecycle states the poller reacts to. The split between
/// `Open(Clean)` and `Open(Conflict)` is the load-bearing addition
/// for the merge-conflict design — they share `state='OPEN'` on the
/// GitHub side and are disambiguated by `mergeable` /
/// `mergeStateStatus`. The `Open` variant carries the joint
/// (mergeability, CI) status (design §Q1's `OpenPrStatus`). `Merged`
/// is what the original poller detected. `ClosedUnmerged` is
/// captured for completeness (per the closed-unmerged design); the
/// current sweep treats it as a no-op (a PR force-deleted out of
/// review is the user's problem, not the poller's), preserving prior
/// behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrStatus),
    Merged,
    ClosedUnmerged,
}

/// Joint mergeability + CI status for an open PR. The two signals
/// share a probe round-trip and a single sweep dispatch (design §Q1's
/// "Composing the CI signal into the same probe"). The merge-poller
/// match expression routes on the pair: a conflict pre-empts CI
/// detection (the conflict-resolver owns the slot first); both clean
/// drives the retire path; CI-only failures fan out to `ci_watch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPrStatus {
    pub mergeability: OpenPrMergeability,
    pub ci: OpenPrCiStatus,
}

impl OpenPrStatus {
    /// Mergeable, CI clean — the steady-state "in_review and happy"
    /// shape. Used both by the production parser and by tests that
    /// only care about one of the two signals.
    pub fn clean() -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the conflict signal
    /// (the corresponding `ci` slot is `Clean`).
    pub fn conflict_only() -> Self {
        Self {
            mergeability: OpenPrMergeability::Conflict,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the CI-failing
    /// signal (the corresponding `mergeability` slot is `Clean`).
    pub fn ci_failing(failures: Vec<RequiredCheckFailure>) -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing { failures },
        }
    }
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

/// CI status of an open PR's required checks at probe time. Derived
/// from `statusCheckRollup` after collapsing by name (latest leaf per
/// check name; design §Q1) and applying the closed failure-conclusion
/// set against required checks only.
///
/// `Clean` means every required check is either `COMPLETED+SUCCESS`,
/// `NEUTRAL`, or `SKIPPED`. `Failing` carries the set of failing
/// required checks for the worker prompt. `InFlight` is the wait
/// state — at least one required check has not reached a terminal
/// conclusion yet; we do not trigger a CI-fix attempt on it (the
/// `auto-retire` path also waits for terminal success across the
/// board, design §Q5 / "Auto-retire" requires *all* checks at SUCCESS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrCiStatus {
    Clean,
    Failing { failures: Vec<RequiredCheckFailure> },
    InFlight,
}

/// One required check that failed at probe time. Captured pre-spawn so
/// the `ci_remediations.failed_checks` JSON is faithful to what the
/// engine saw and the worker prompt embeds the same data.
///
/// `conclusion` is GitHub's value (`FAILURE`, `TIMED_OUT`, `CANCELLED`,
/// `STARTUP_FAILURE`, `ACTION_REQUIRED`, `STALE`). `target_url` points
/// at the provider's job page; `provider` is inferred from its host;
/// `provider_job_id` is parsed from the URL when possible and `None`
/// when the format is unrecognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheckFailure {
    pub name: String,
    pub conclusion: String,
    pub target_url: String,
    pub provider: CiProvider,
    pub provider_job_id: Option<String>,
}

/// CI provider inferred from a check's `targetUrl` host. The CI-watch
/// `CiLogReader` impls (Buildkite + GitHub Actions) dispatch on this
/// when they ship; the `Other` variant captures anything we don't
/// know how to read (status contexts from third-party services like
/// Codecov, Sonar, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiProvider {
    Buildkite,
    GithubActions,
    Other,
}

/// Closed set of conclusion strings that count as "failure" for the
/// required-check predicate (design §Q1). `ACTION_REQUIRED` is a
/// special case: the worker can't approve manual workflows, so we
/// surface it as a failure but the engine's pre-triage immediately
/// flags it `manual_action_required` (design §Q4). `ERROR` is the
/// legacy-commit-status equivalent of `FAILURE` (StatusContext leaves
/// — see [`normalize_leaf`]) and lands in the same bucket.
fn is_failure_conclusion(c: &str) -> bool {
    matches!(
        c.to_ascii_uppercase().as_str(),
        "FAILURE"
            | "ERROR"
            | "TIMED_OUT"
            | "CANCELLED"
            | "STARTUP_FAILURE"
            | "ACTION_REQUIRED"
            | "STALE"
    )
}

/// Closed set of conclusion strings that count as "successful enough
/// to ignore" for the required-check predicate. `NEUTRAL` and
/// `SKIPPED` do not gate merge per branch protection; `SUCCESS` is
/// the happy path.
fn is_pass_conclusion(c: &str) -> bool {
    matches!(
        c.to_ascii_uppercase().as_str(),
        "SUCCESS" | "NEUTRAL" | "SKIPPED",
    )
}

/// Infer the CI provider from a check's `targetUrl` host.
fn provider_for_url(url: &str) -> CiProvider {
    if url.is_empty() {
        return CiProvider::Other;
    }
    let lower = url.to_ascii_lowercase();
    if lower.contains("buildkite.com") {
        return CiProvider::Buildkite;
    }
    // GitHub Actions URLs look like:
    //   https://github.com/<owner>/<repo>/actions/runs/<run-id>/job/<job-id>
    // (or the older /check-runs/ form). Either format → GHA.
    if lower.contains("github.com") && (lower.contains("/actions/") || lower.contains("/check-runs/"))
    {
        return CiProvider::GithubActions;
    }
    CiProvider::Other
}

/// Extract the provider's job id from a `targetUrl`. Buildkite job
/// ids ride in the URL fragment (`…/builds/<n>#<job-uuid>`); GitHub
/// Actions job ids are the last path segment after `/job/`. Returns
/// `None` for URLs that don't match either pattern — the worker
/// prompt then shows the raw URL and the worker shells out manually.
fn parse_provider_job_id(provider: CiProvider, url: &str) -> Option<String> {
    match provider {
        CiProvider::Buildkite => url.split_once('#').map(|(_, frag)| frag.to_owned()),
        CiProvider::GithubActions => {
            // …/actions/runs/<run-id>/job/<job-id>[?…]
            let stripped = url.split('?').next().unwrap_or(url);
            stripped
                .rsplit_once("/job/")
                .map(|(_, tail)| tail.trim_end_matches('/').to_owned())
        }
        CiProvider::Other => None,
    }
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

/// `MergeProbe` that always returns an error — used as the default in
/// contexts that do not need real GitHub probing (e.g. unit tests that
/// never reach the CI-fetch path).
#[derive(Debug, Default)]
pub struct NoopMergeProbe;

#[async_trait]
impl MergeProbe for NoopMergeProbe {
    async fn probe(&self, _pr_url: &str) -> Result<PrLifecycleProbe> {
        anyhow::bail!("NoopMergeProbe: no real probe configured")
    }
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
                // `statusCheckRollup` is a nested array we parse in
                // Rust (design §Q1's "Composing the CI signal into
                // the same probe"); the previous TSV-via-jq shape
                // can't carry it without escaping headaches, so we
                // take the raw JSON document from gh instead.
                // `reviewDecision` and `reviews` are added to capture
                // the review-required state for UI indicators.
                // NOTE: `mergeQueueEntry` is intentionally omitted here —
                // `gh pr view --json` does not expose it in all `gh` versions.
                // Merge-queue state is queried separately via `gh api graphql`
                // in `fetch_merge_queue_status` below.
                "state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,headRefOid,headRefName,baseRefName,labels,statusCheckRollup,reviewDecision,reviews",
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
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: PrReviewState::Unknown,
                    in_merge_queue: false,
                });
            }
            return Err(anyhow!(
                "`gh pr view {pr_url}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // When `statusCheckRollup` is empty the GraphQL field omits
        // required-but-unstarted status contexts ("EXPECTED" in GitHub's
        // web UI). The legacy commit-status REST endpoint returns
        // `state:"pending"` in that case, which lets us show a non-green
        // indicator instead of a false-positive green.
        let combined_state =
            fetch_commit_combined_state_for_empty_rollup(&stdout, pr_url).await;
        let mut probe = parse_probe_json(pr_url, &stdout, combined_state.as_deref())?;
        // Query merge-queue status separately via GraphQL since `gh pr view --json`
        // does not expose `mergeQueueEntry` in all installed `gh` versions.
        probe.in_merge_queue = fetch_merge_queue_status(pr_url).await;
        Ok(probe)
    }
}

/// Extract `"owner/repo"` from a GitHub PR URL of the form
/// `https://github.com/owner/repo/pull/NNN`.
fn repo_from_pr_url(pr_url: &str) -> Option<&str> {
    let path = pr_url.strip_prefix("https://github.com/")?;
    let mut segments = path.splitn(3, '/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let end = owner.len() + 1 + repo.len();
    Some(&path[..end])
}

/// Extract the PR number from a GitHub PR URL of the form
/// `https://github.com/owner/repo/pull/NNN`.
fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    let path = pr_url.strip_prefix("https://github.com/")?;
    // path is now "owner/repo/pull/NNN" or similar
    let mut segments = path.splitn(4, '/');
    segments.next()?; // owner
    segments.next()?; // repo
    let pull = segments.next()?;
    if pull != "pull" {
        return None;
    }
    segments.next()?.parse().ok()
}

/// Query GitHub's GraphQL API to determine whether `pr_url` is currently
/// in the repository's merge queue. Returns `true` when `mergeQueueEntry`
/// is non-null (the PR is queued), `false` on any error or when not queued.
///
/// This is a separate call from the main `gh pr view` probe because
/// `mergeQueueEntry` is not exposed as a `--json` field in all installed
/// versions of the `gh` CLI. The GraphQL API is stable and available across
/// versions.
async fn fetch_merge_queue_status(pr_url: &str) -> bool {
    let (Some(owner_repo), Some(number)) = (repo_from_pr_url(pr_url), pr_number_from_url(pr_url)) else {
        return false;
    };
    let (owner, repo) = match owner_repo.split_once('/') {
        Some(pair) => pair,
        None => return false,
    };
    let query = format!(
        r#"{{ repository(owner: "{owner}", name: "{repo}") {{ pullRequest(number: {number}) {{ mergeQueueEntry {{ state }} }} }} }}"#
    );
    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let body: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // data.repository.pullRequest.mergeQueueEntry — non-null → in queue.
    !body["data"]["repository"]["pullRequest"]["mergeQueueEntry"].is_null()
}

/// One `RemovedFromMergeQueueEvent` entry from the PR's timeline.
#[derive(Debug, Clone)]
pub struct MergeQueueDequeueEvent {
    pub reason: String,
    /// `beforeCommit.oid` — the synthetic merge SHA that failed CI.
    /// `None` when GitHub omitted it (edge case for non-CI reasons).
    pub before_commit_oid: Option<String>,
}

/// Query the PR's timeline for `RemovedFromMergeQueueEvent` entries.
/// Returns events with `reason == "failed_checks"` (case-insensitive;
/// GitHub's API returns the lowercase form even though the GraphQL schema
/// documents the enum as uppercase `FAILED_CHECKS`). Events for other
/// reasons (`MANUAL_REMOVAL`, `MERGE_CONFLICT`, etc.) are filtered out.
///
/// Returns an empty vec on any error so the sweep degrades gracefully.
/// The `INSERT OR IGNORE` idempotency on `ci_remediations` deduplicates
/// re-seen events across sweeps without any extra tracking.
async fn fetch_merge_queue_dequeue_events(pr_url: &str) -> Vec<MergeQueueDequeueEvent> {
    let (Some(owner_repo), Some(number)) = (repo_from_pr_url(pr_url), pr_number_from_url(pr_url)) else {
        return Vec::new();
    };
    let (owner, repo) = match owner_repo.split_once('/') {
        Some(pair) => pair,
        None => return Vec::new(),
    };
    // Query the last 20 timeline items — enough to cover any realistically
    // plausible burst of re-enqueue/dequeue cycles on a single PR.
    let query = format!(
        r#"{{ repository(owner: "{owner}", name: "{repo}") {{ pullRequest(number: {number}) {{ timelineItems(itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT], last: 20) {{ nodes {{ ... on RemovedFromMergeQueueEvent {{ reason beforeCommit {{ oid }} }} }} }} }} }} }}"#
    );
    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    parse_dequeue_events_response(&output.stdout)
}

/// Pure parser for the GraphQL `timelineItems` response body from
/// [`fetch_merge_queue_dequeue_events`]. Extracted so the parsing rules
/// can be unit-tested without a live `gh` call.
///
/// GitHub's API returns `reason` in lowercase snake_case (e.g.
/// `"failed_checks"`) even though the GraphQL enum is documented in
/// uppercase (`FAILED_CHECKS`). The filter uses a case-insensitive
/// comparison so both forms are accepted.
fn parse_dequeue_events_response(body: &[u8]) -> Vec<MergeQueueDequeueEvent> {
    let body: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let nodes = match body["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
        .as_array()
    {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut events = Vec::new();
    for node in nodes {
        let reason = match node["reason"].as_str() {
            Some(r) => r.to_owned(),
            None => continue,
        };
        // Only surface FAILED_CHECKS — all other reasons are informational
        // or terminal-success and must not feed the ci_failure path.
        // GitHub returns the lowercase form "failed_checks" even though
        // the schema declares the enum as FAILED_CHECKS; compare
        // case-insensitively to accept both.
        if !reason.eq_ignore_ascii_case("failed_checks") {
            continue;
        }
        let before_commit_oid = node["beforeCommit"]["oid"].as_str().map(|s| s.to_owned());
        events.push(MergeQueueDequeueEvent { reason, before_commit_oid });
    }
    events
}

/// When `statusCheckRollup` is empty/null in `json_body`, fetches the
/// legacy commit-status combined state (`pending` / `success` / `failure`
/// / `error`) from GitHub's REST endpoint and returns it as a lowercase
/// string. Returns `None` on any error, when the rollup is non-empty
/// (the caller should rely on rollup data in that case), or when the
/// commit has zero recorded statuses — GitHub reports `state:"pending"`
/// even when `total_count == 0`, which would otherwise show up as a stuck
/// yellow "waiting for CI" icon on PRs in repos with no checks configured.
async fn fetch_commit_combined_state_for_empty_rollup(
    json_body: &str,
    pr_url: &str,
) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(json_body.trim()).ok()?;
    let rollup = root.get("statusCheckRollup").and_then(|v| v.as_array())?;
    if !rollup.is_empty() {
        return None; // non-empty rollup; use rollup data
    }
    let head_sha = root
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let repo = repo_from_pr_url(pr_url)?;
    let api_path = format!("repos/{repo}/commits/{head_sha}/status");
    let output = Command::new("gh")
        .args([
            "api",
            &api_path,
            "--jq",
            "{state: .state, total_count: .total_count}",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let body = std::str::from_utf8(&output.stdout).ok()?;
    parse_combined_status_response(body)
}

/// Pure parser for GitHub's `repos/{owner}/{repo}/commits/{sha}/status`
/// response shape (`{state, total_count}`). A commit with zero recorded
/// statuses reports `state:"pending"` even though there is nothing to
/// wait on — keying on `total_count` collapses that case to `None` so
/// the caller treats the PR as `Clean` instead of stuck in-flight.
fn parse_combined_status_response(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let total_count = v.get("total_count").and_then(|t| t.as_u64()).unwrap_or(0);
    if total_count == 0 {
        return None;
    }
    let state = v
        .get("state")
        .and_then(|s| s.as_str())?
        .trim()
        .to_ascii_lowercase();
    if state.is_empty() { None } else { Some(state) }
}

/// Parse the raw JSON document `gh pr view --json …` returns into a
/// [`PrLifecycleProbe`]. Pure function so the parsing rules can be
/// unit-tested without shelling out. A document that fails to parse
/// is *not* treated as conflicting / failing — we fall back to an
/// `Open(clean)` shape so a malformed gh response can't fire a
/// false-positive blocked flip. Real failures (auth, network) come
/// through as `Err` from the shelling-out layer, not via this path.
///
/// `combined_state` is the optional result from the legacy commit-status
/// REST API (`pending` / `success` / `failure` / `error`). It is only
/// consulted when `statusCheckRollup` is empty — see
/// [`fetch_commit_combined_state_for_empty_rollup`].
fn parse_probe_json(url: &str, body: &str, combined_state: Option<&str>) -> Result<PrLifecycleProbe> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "`gh pr view {url}` returned an empty document"
        ));
    }
    let root: serde_json::Value = serde_json::from_str(trimmed)
        .with_context(|| format!("failed to parse `gh pr view {url}` JSON"))?;
    let raw_state = root.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let merged_at = root.get("mergedAt").and_then(|v| v.as_str()).unwrap_or("");
    let mergeable = root.get("mergeable").and_then(|v| v.as_str()).unwrap_or("");
    let merge_state_status = root
        .get("mergeStateStatus")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let base_ref_oid = root
        .get("baseRefOid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let head_ref_oid = root
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let head_ref_name = root
        .get("headRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let base_ref_name = root
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let labels = root
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let rollup = root
        .get("statusCheckRollup")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Per-org reclassification: a status check that GitHub reports as a
    // required CI check but that semantically gates merge on a human
    // approval signal (e.g. LinkedIn's `Owner Approval` / LI-ACL) is
    // partitioned out of the rollup before CI classification and fed
    // into the review-signal axis instead. Outside the configured orgs
    // the partition is a no-op and the rollup is classified normally.
    let owner = owner_from_pr_url(url).unwrap_or("");
    let review_signal_names = review_signal_checks_for_owner(owner);
    let (review_signal_leaves, ci_leaves): (Vec<serde_json::Value>, Vec<serde_json::Value>) = rollup
        .into_iter()
        .partition(|leaf| leaf_matches_check_name(leaf, review_signal_names));
    let ci = classify_ci(&ci_leaves, combined_state);
    let state = classify_state(raw_state, merged_at, mergeable, merge_state_status, ci);
    let review_signal = classify_review_signal(&review_signal_leaves);
    let review_decision = root
        .get("reviewDecision")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let reviews = root
        .get("reviews")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let review = classify_review(review_decision, &reviews, review_signal);
    // `mergeQueueEntry` is non-null when the PR is in GitHub's merge queue.
    // Null, missing, or explicit JSON null → not in queue.
    let in_merge_queue = root
        .get("mergeQueueEntry")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    Ok(PrLifecycleProbe {
        url: url.to_owned(),
        state,
        base_ref_oid,
        head_ref_oid,
        head_ref_name,
        base_ref_name,
        labels,
        review,
        in_merge_queue,
    })
}

/// Derive the [`PrReviewState`] from GitHub's `reviewDecision` string,
/// the `reviews` array, and an optional per-org review-signal verdict
/// produced from reclassified status checks (e.g. LinkedIn's
/// `Owner Approval`). Rules for the GitHub portion:
///
///   - `REVIEW_REQUIRED` → `Required` (no reviewers needed yet).
///   - `CHANGES_REQUESTED` → `ChangesRequested`; reviewers are the
///     latest CHANGES_REQUESTED submitters per author (de-duped).
///   - `APPROVED` → `Approved`; reviewers are the latest APPROVED
///     submitters per author (de-duped).
///   - Empty / `null` / unrecognised → `Unknown` (no branch
///     protection or first poll hasn't run). The UI hides the
///     indicator in this case rather than showing a misleading green.
///
/// `review_signal` then overlays per the dominance rule:
///   - `Pass` / `None` → no override; the GitHub verdict stands.
///   - `InFlight` → force `Required` unless the GitHub verdict is
///     `ChangesRequested` (a stronger negative signal we preserve).
///   - `Fail` → force `ChangesRequested { reviewers: [] }`. An ACL
///     rejection is conceptually "approval refused" but the rollup
///     leaf carries no reviewer identity, so we leave the list empty.
fn classify_review(
    review_decision: &str,
    reviews: &[serde_json::Value],
    review_signal: ReviewSignalVerdict,
) -> PrReviewState {
    // Collect the most-recent review state per author from the
    // `reviews` array. GitHub orders reviews oldest-to-newest so
    // iterating forward and overwriting gives us the latest per author.
    let mut by_author: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for review in reviews {
        let login = review
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let state = review
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        if !login.is_empty() && !state.is_empty() {
            by_author.insert(login, state);
        }
    }

    let base = match review_decision.to_ascii_uppercase().as_str() {
        "REVIEW_REQUIRED" => PrReviewState::Required,
        "CHANGES_REQUESTED" => {
            let reviewers = by_author
                .into_iter()
                .filter(|(_, state)| state == "CHANGES_REQUESTED")
                .map(|(login, _)| login)
                .collect();
            PrReviewState::ChangesRequested { reviewers }
        }
        "APPROVED" => {
            let reviewers = by_author
                .into_iter()
                .filter(|(_, state)| state == "APPROVED")
                .map(|(login, _)| login)
                .collect();
            PrReviewState::Approved { reviewers }
        }
        _ => PrReviewState::Unknown,
    };
    apply_review_signal(base, review_signal)
}

/// Apply a per-org review-signal verdict over the base GitHub review
/// state. `None` / `Pass` are no-ops; `InFlight` forces `Required`
/// unless the base already says `ChangesRequested`; `Fail` forces
/// `ChangesRequested { reviewers: [] }` (the leaf carries no identity).
fn apply_review_signal(base: PrReviewState, signal: ReviewSignalVerdict) -> PrReviewState {
    match signal {
        ReviewSignalVerdict::None | ReviewSignalVerdict::Pass => base,
        ReviewSignalVerdict::InFlight => match base {
            PrReviewState::ChangesRequested { .. } => base,
            _ => PrReviewState::Required,
        },
        ReviewSignalVerdict::Fail => PrReviewState::ChangesRequested { reviewers: Vec::new() },
    }
}

/// Verdict on a per-org "review signal" status check, after
/// [`normalize_leaf`]'s buckets are folded across all reclassified
/// leaves. `None` means no reclassified check is present on the PR
/// (the common case — non-LinkedIn org, or the check is absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewSignalVerdict {
    None,
    /// At least one reclassified check is still running.
    InFlight,
    /// All reclassified checks have completed successfully.
    Pass,
    /// At least one reclassified check has failed/errored.
    Fail,
}

/// Per-org table of status-check `context` names that are reclassified
/// from CI signals to review signals. Match is case-insensitive on
/// both axes. v1 hardcodes the two LinkedIn orgs known to ship the
/// `Owner Approval` (LI-ACL) check; the table shape is deliberately
/// extensible so adding more orgs (or more check names per org) later
/// is a one-line change rather than another aggregation-layer hook.
const REVIEW_SIGNAL_RULES: &[(&str, &[&str])] = &[
    ("linkedin-multiproduct", &["Owner Approval"]),
    ("linkedin-eng", &["Owner Approval"]),
];

/// The list of status-check `context` names to reclassify for `owner`.
/// Empty slice for unconfigured owners — the call site partitions on
/// that and the rollup is classified normally.
///
/// `pub(crate)` so the worker-prompt composer (`runner.rs`) can name the
/// same human-gated checks the CI classifier here reclassifies. That
/// single sourcing is the point of issue #899: the worker's
/// "don't wait on these checks" guidance and the engine's
/// "these checks don't block CI-clean" detection must not drift apart.
pub(crate) fn review_signal_checks_for_owner(owner: &str) -> &'static [&'static str] {
    for (org, names) in REVIEW_SIGNAL_RULES {
        if org.eq_ignore_ascii_case(owner) {
            return names;
        }
    }
    &[]
}

/// Extract just the `<owner>` segment from a GitHub PR URL of the
/// form `https://github.com/<owner>/<repo>/pull/<n>`. Returns `None`
/// when the URL does not match the GitHub PR shape.
fn owner_from_pr_url(pr_url: &str) -> Option<&str> {
    let repo = repo_from_pr_url(pr_url)?;
    Some(repo.split_once('/')?.0)
}

/// Whether a rollup leaf's check name (the `name` field on a CheckRun
/// or the `context` field on a StatusContext) matches any of `names`
/// case-insensitively. An empty `names` slice yields `false` without
/// inspecting the leaf, so the common no-reclassification path costs
/// one branch.
fn leaf_matches_check_name(leaf: &serde_json::Value, names: &[&str]) -> bool {
    if names.is_empty() {
        return false;
    }
    let leaf_name = leaf
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| leaf.get("context").and_then(|v| v.as_str()))
        .unwrap_or("");
    if leaf_name.is_empty() {
        return false;
    }
    names.iter().any(|n| n.eq_ignore_ascii_case(leaf_name))
}

/// Fold the partitioned review-signal leaves into one
/// [`ReviewSignalVerdict`] via [`normalize_leaf`]'s buckets.
/// Fail dominates InFlight which dominates Pass; an empty input
/// (the common case) → `None`.
fn classify_review_signal(leaves: &[serde_json::Value]) -> ReviewSignalVerdict {
    if leaves.is_empty() {
        return ReviewSignalVerdict::None;
    }
    let mut any_in_flight = false;
    let mut any_fail = false;
    for leaf in leaves {
        match normalize_leaf(leaf) {
            LeafVerdict::Fail { .. } => any_fail = true,
            LeafVerdict::InFlight => any_in_flight = true,
            LeafVerdict::Pass => {}
        }
    }
    if any_fail {
        ReviewSignalVerdict::Fail
    } else if any_in_flight {
        ReviewSignalVerdict::InFlight
    } else {
        ReviewSignalVerdict::Pass
    }
}

/// Verdict bucket a single rollup leaf contributes to. Produced by
/// [`normalize_leaf`] so the two GraphQL leaf shapes
/// (`CheckRun` and `StatusContext`) feed the same downstream branches
/// in [`classify_ci`].
enum LeafVerdict {
    /// Leaf is in a non-terminal state (queued / running / expected /
    /// briefly post-completion with empty conclusion).
    InFlight,
    /// Leaf reached a successful terminal state (`SUCCESS` /
    /// `NEUTRAL` / `SKIPPED`).
    Pass,
    /// Leaf reached a failing terminal state. `conclusion` is the
    /// uppercased token kept verbatim for the worker prompt /
    /// `ci_remediations.failed_checks` JSON.
    Fail { conclusion: String },
}

/// Normalize one rollup leaf into a [`LeafVerdict`]. `gh pr view
/// --json statusCheckRollup` returns a heterogeneous array containing
/// two GraphQL types:
///
///   - `CheckRun` — modern check-runs (GitHub Actions, most CI
///     integrations). Carries `name`, `status`, `conclusion`.
///   - `StatusContext` — the legacy commit-status API shape (Buildkite,
///     some self-hosted CI). Carries `context`, `state`. **No** `status`
///     or `conclusion` field.
///
/// Treating the two uniformly via `status`+`conclusion` (the pre-fix
/// behaviour) silently classifies every StatusContext leaf as InFlight
/// because both fields read empty, which is why a green Buildkite-only
/// PR stayed pinned on the yellow-clock badge indefinitely.
fn normalize_leaf(leaf: &serde_json::Value) -> LeafVerdict {
    let typename = leaf
        .get("__typename")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // StatusContext: `state` carries the verdict. Values per GitHub's
    // commit-status API: SUCCESS / FAILURE / ERROR / PENDING / EXPECTED.
    // Dispatch on `__typename` when present; fall back to "has `state`
    // but no `conclusion`" so older fixtures (and any future leaf shape
    // that mirrors StatusContext) classify correctly.
    let has_status_context_shape = typename.eq_ignore_ascii_case("StatusContext")
        || (leaf.get("state").is_some() && leaf.get("conclusion").is_none());
    if has_status_context_shape {
        let state = leaf
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        return match state.as_str() {
            "SUCCESS" => LeafVerdict::Pass,
            "FAILURE" | "ERROR" => LeafVerdict::Fail { conclusion: state },
            // PENDING (running), EXPECTED (branch protection lists the
            // context but no run has reported yet), empty, or anything
            // else GitHub may add later → wait for a terminal verdict.
            _ => LeafVerdict::InFlight,
        };
    }

    // CheckRun (and unknown typenames that still carry CheckRun-shaped
    // fields): combine `status` and `conclusion`. A leaf is in-flight
    // when its status is one of GitHub's pending-shape values OR when
    // the conclusion is still empty (briefly, post-completion).
    let status = leaf
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let conclusion = leaf
        .get("conclusion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let status_in_flight = matches!(
        status.as_str(),
        "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" | ""
    );
    if conclusion.is_empty() {
        return LeafVerdict::InFlight;
    }
    if is_failure_conclusion(&conclusion) {
        return LeafVerdict::Fail { conclusion };
    }
    if is_pass_conclusion(&conclusion) {
        return LeafVerdict::Pass;
    }
    if status_in_flight {
        return LeafVerdict::InFlight;
    }
    // Unknown conclusion shape — treat as in-flight rather than
    // misclassifying it as a failure.
    LeafVerdict::InFlight
}

/// Collapse the `statusCheckRollup` array into one [`OpenPrCiStatus`]
/// per the design's §Q1 predicate:
///
///   1. Drop leaves where `isRequired` is explicitly `false`. Leaves
///      that don't report `isRequired` (legacy status contexts,
///      providers that don't fill the field) default to `true` —
///      branch protection is the authority, and we'd rather over-trip
///      on a third-party check than ignore a real signal.
///   2. Group by check name; pick the latest leaf per name (we use the
///      last entry, which matches GitHub's natural ordering for
///      re-runs — the most recent run lands last in the rollup).
///   3. For each surviving leaf, run [`normalize_leaf`] to fold the
///      two leaf shapes (`CheckRun` and `StatusContext`) into a single
///      verdict bucket.
///   4. If any failures collected → `Failing`. Else if any leaf was
///      InFlight → `InFlight`. Else if rollup was empty, consult
///      `combined_state` from the legacy commit-status REST API:
///        - `"pending"` / `"failure"` / `"error"` → `InFlight`
///          (required contexts configured but not yet submitted).
///        - `"success"` or absent → `Clean` (no required checks).
///
/// `combined_state` is only consulted when `leaves` is empty; for a
/// non-empty rollup the leaf data is authoritative.
fn classify_ci(leaves: &[serde_json::Value], combined_state: Option<&str>) -> OpenPrCiStatus {
    use std::collections::BTreeMap;

    // Group by name, keeping the most-recently-seen leaf per name.
    // The rollup is ordered oldest-to-newest for same-name re-runs.
    let mut by_name: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for leaf in leaves {
        // `isRequired` defaults to `true` when missing; only filter
        // out the explicit `false`.
        let required = leaf
            .get("isRequired")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !required {
            continue;
        }
        let name = leaf
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| leaf.get("context").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_owned();
        if name.is_empty() {
            continue;
        }
        by_name.insert(name, leaf);
    }

    let mut failures: Vec<RequiredCheckFailure> = Vec::new();
    let mut any_in_flight = false;
    for (name, leaf) in by_name {
        match normalize_leaf(leaf) {
            LeafVerdict::Pass => {}
            LeafVerdict::InFlight => {
                any_in_flight = true;
            }
            LeafVerdict::Fail { conclusion } => {
                let target_url = leaf
                    .get("targetUrl")
                    .and_then(|v| v.as_str())
                    .or_else(|| leaf.get("detailsUrl").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_owned();
                let provider = provider_for_url(&target_url);
                let provider_job_id = parse_provider_job_id(provider, &target_url);
                failures.push(RequiredCheckFailure {
                    name,
                    conclusion,
                    target_url,
                    provider,
                    provider_job_id,
                });
            }
        }
    }

    if !failures.is_empty() {
        return OpenPrCiStatus::Failing { failures };
    }
    if any_in_flight {
        return OpenPrCiStatus::InFlight;
    }
    // No check-run data in the rollup. Consult the legacy commit-status
    // combined state when available: "pending" means required status
    // contexts are configured in branch protection but haven't been
    // submitted yet (GitHub's web UI labels this "Expected"). Treat any
    // non-success combined state as InFlight so the kanban card shows a
    // waiting indicator instead of a false-positive green checkmark.
    if leaves.is_empty() {
        match combined_state
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("pending") | Some("failure") | Some("error") => {
                return OpenPrCiStatus::InFlight;
            }
            _ => {}
        }
    }
    OpenPrCiStatus::Clean
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
///     The `ci` axis is supplied by the caller from
///     [`classify_ci`] — both axes share the `Open` wrapper.
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
    ci: OpenPrCiStatus,
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
    let mergeability = if conflicting {
        OpenPrMergeability::Conflict
    } else {
        OpenPrMergeability::Clean
    };
    PrLifecycleState::Open(OpenPrStatus { mergeability, ci })
}

/// Outcome of one sweep. Used for logging and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepOutcome {
    pub merged: usize,
    pub conflict_flagged: usize,
    pub conflict_cleared: usize,
    pub ci_flagged: usize,
    pub ci_cleared: usize,
    /// Number of `waiting_human` executions whose chore was missing a
    /// `pr_url` but whose workspace now resolves to a fresh PR. These
    /// are the rows the on-Stop hook missed (typically because GitHub's
    /// `commits/{sha}/pulls` index lagged a fresh `gh pr create`). The
    /// recheck moved them to `in_review` (or `done` if the PR was
    /// already merged).
    pub pr_recheck_recovered: usize,
    /// Number of `waiting_human` executions where this sweep ran a
    /// recheck but the detector still did not resolve a bindable PR
    /// (returned `None`, `Stale`, `EmptyDiff`, or errored). Mirrors
    /// the info-level log in `sweep_pending_pr` so callers (and tests)
    /// can assert the recheck path actually reached the executions in
    /// its candidate list, even when no transition fired.
    pub pr_recheck_unresolved: usize,
    /// Number of `in_review` PRs flipped to `blocked: ci_failure` due to
    /// a merge-queue `FAILED_CHECKS` dequeue event detected in this sweep.
    pub merge_queue_rebounced: usize,
    /// Number of stranded `ci_remediations` attempts (status `pending`,
    /// no live execution) for which a fresh execution was re-emitted.
    /// Covers the back-to-back dequeue scenario where two dequeue events
    /// arrive in the same sweep: the first flips the task (consuming the
    /// WHERE guard) and the second inserts a ci_remediations row but
    /// cannot create an execution because the task is already blocked.
    pub ci_remediation_redispatched: usize,
    /// Number of terminal executions (abandoned/completed/failed within
    /// the lookback window) whose task was still `active` with no `pr_url`
    /// but now has a detectable PR. These arise from the double-spawn race
    /// (Bug B): exec_A was abandoned while its pane was still running, and
    /// the normal `pending_pr_recheck` sweep (which only watches
    /// `waiting_human`) cannot recover them.
    pub late_pr_recovered: usize,
    /// Number of in-flight revision executions stopped (force-released +
    /// cancelled) because their parent PR merged or closed while they were
    /// queued or running. Each stopped execution corresponds to a revision
    /// task that was already blocked in the same DB transaction that
    /// transitioned the parent to `done`.
    pub revision_invalidated: usize,
    /// Number of live worker executions force-stopped because their task
    /// auto-transitioned back to `in_review` after the engine detected
    /// the PR's CI had gone green. The worker (typically still polling CI
    /// to see whether its own fix landed) has nothing useful left to do
    /// once the task reaches Review, so leaving it alive only ties up a
    /// slot (issue #898).
    pub worker_stopped_on_review: usize,
}

impl SweepOutcome {
    fn total_transitions(self) -> usize {
        self.merged
            + self.conflict_flagged
            + self.conflict_cleared
            + self.ci_flagged
            + self.ci_cleared
            + self.pr_recheck_recovered
            + self.merge_queue_rebounced
            + self.ci_remediation_redispatched
            + self.late_pr_recovered
            + self.revision_invalidated
            + self.worker_stopped_on_review
    }
}

/// Run one full lifecycle sweep over every chore and project_task
/// the poller cares about (in_review with a PR, plus rows currently
/// blocked on merge_conflict so we can detect resolution, plus
/// `waiting_human` executions whose chore is still missing a
/// `pr_url`). Returns per-bucket counters so callers can log a
/// one-line summary.
///
/// `cube_client` is threaded into the conflict-watch retire path so
/// `on_resolved` can release the cube workspace lease the resolution
/// worker held (design Q5). Pass `None` for sweeps that don't need to
/// drive lease release — pre-Phase-3 wiring, tests, etc.
///
/// `completion_handler` is threaded in so the pending-PR-detection
/// recheck can reuse the on-Stop transition path (`record_worker_pr_completion`
/// + cube release + pane teardown + event publish). Pass `None` for
/// pre-`completion_handler` wiring and tests that exercise only the
/// in-review and conflict paths.
pub async fn run_one_pass(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&WorkerCompletionHandler>,
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
    let blocked_ci = match work_db.list_chores_blocked_on_ci_failure() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list chores blocked on ci_failure",
            );
            Vec::new()
        }
    };
    let pending_pr_recheck = match work_db.list_executions_pending_pr_detection() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list executions pending PR detection",
            );
            Vec::new()
        }
    };
    let stranded_ci_attempts = match work_db.list_stranded_ci_remediation_attempts() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list stranded ci remediation attempts",
            );
            Vec::new()
        }
    };
    // Late-PR candidates (Bug B recovery): terminal executions within
    // the last 60 min whose task is still `active` with no `pr_url`.
    // These arise from the double-spawn race where the orphan sweep
    // abandons exec_A while its pane is still running. The normal
    // `pending_pr_recheck` sweep (which only watches `waiting_human`)
    // cannot recover them; this sweep fills the gap.
    let late_pr_candidates: Vec<LatePrCandidate> = if completion_handler.is_some() {
        match work_db.list_recently_terminal_executions_pending_pr_detection(3600) {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "merge poller: failed to list late PR candidates",
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let total = in_review.len()
        + blocked_conflict.len()
        + blocked_ci.len()
        + pending_pr_recheck.len()
        + stranded_ci_attempts.len()
        + late_pr_candidates.len();
    if total == 0 {
        return SweepOutcome::default();
    }
    tracing::debug!(
        in_review = in_review.len(),
        blocked_conflict = blocked_conflict.len(),
        blocked_ci = blocked_ci.len(),
        pending_pr_recheck = pending_pr_recheck.len(),
        stranded_ci_attempts = stranded_ci_attempts.len(),
        late_pr_candidates = late_pr_candidates.len(),
        "merge poller: sweep started",
    );
    let mut outcome = SweepOutcome::default();
    // De-duplicate by work_item_id: a chore that's both pending and
    // blocked-on-CI (shouldn't happen but defensive) only gets one
    // probe per sweep.
    let mut seen = std::collections::HashSet::new();
    for candidate in in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
    {
        if !seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        sweep_one(work_db, probe, publisher, cube_client, completion_handler, candidate, &mut outcome).await;
    }
    if let Some(handler) = completion_handler {
        for execution_id in &pending_pr_recheck {
            sweep_pending_pr(handler, execution_id, &mut outcome).await;
        }
    } else if !pending_pr_recheck.is_empty() {
        tracing::debug!(
            count = pending_pr_recheck.len(),
            "merge poller: pending PR-detection candidates skipped (no completion_handler wired)",
        );
    }
    // Rescue stranded ci_remediations attempts: `pending` rows with no live
    // execution. These arise when two dequeue events land in the same sweep —
    // the first flips the task (consuming the WHERE guard on
    // `mark_chore_blocked_ci_failure`) and the second inserts a ci_remediations
    // row but cannot create an execution. Re-emit a fresh execution so a worker
    // is dispatched without waiting for the task to return to `in_review`.
    for attempt in &stranded_ci_attempts {
        if ci_watch::rescue_stranded_ci_remediation_attempt(work_db, publisher, attempt).await {
            outcome.ci_remediation_redispatched += 1;
        }
    }
    // Late-PR sweep (Bug B): recover terminal executions whose pane
    // pushed a PR after the execution was marked abandoned.
    if let Some(handler) = completion_handler {
        for candidate in &late_pr_candidates {
            sweep_late_pr(handler, candidate, &mut outcome).await;
        }
    }
    // Merge-queue rebounce pass: for every `in_review` PR and every
    // `blocked: ci_failure` PR, poll the GitHub timeline for
    // `RemovedFromMergeQueueEvent` rows with `reason=FAILED_CHECKS`.
    // This is a separate pass from the probe loop above — the probe
    // covers per-PR CI and merge-conflict signals, while this pass
    // specifically looks for queue dequeues. Including `blocked_ci`
    // candidates ensures that a second dequeue (on a PR already blocked
    // by a prior dequeue) inserts a ci_remediations row so the stranded
    // rescue above can dispatch an execution for it.
    // The `INSERT OR IGNORE` idempotency on `ci_remediations` ensures
    // that events already processed on a prior sweep are no-ops.
    let mut rebounce_seen = std::collections::HashSet::new();
    for candidate in in_review.iter().chain(blocked_ci.iter()) {
        if !rebounce_seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        check_merge_queue_rebounce(work_db, publisher, candidate, &mut outcome).await;
    }
    outcome
}

/// Stop every in-flight `revision_implementation` execution belonging to
/// revisions of `chain_root_id` now that the parent PR has merged.
///
/// The DB transaction in `mark_chore_pr_merged` already blocked the
/// revision tasks (via `block_pending_revisions_on_parent_close`).  This
/// function handles the execution side: force-release each cube workspace
/// lease so the slot is freed, then cancel the execution row so the
/// dispatcher treats it as terminal.
///
/// When `completion_handler` is `None` (tests, cold-path wiring) this
/// function is a no-op; the tasks are already blocked in the DB, and the
/// scheduler will not redispatch them on the next reconcile cycle.
async fn stop_active_revision_executions(
    work_db: &WorkDb,
    completion_handler: Option<&WorkerCompletionHandler>,
    chain_root_id: &str,
    outcome: &mut SweepOutcome,
) {
    let Some(handler) = completion_handler else {
        return;
    };
    let executions = match work_db.list_active_revision_executions_for_chain(chain_root_id) {
        Ok(execs) => execs,
        Err(err) => {
            tracing::warn!(
                chain_root_id,
                ?err,
                "merge poller: failed to list active revision executions for chain; \
                 revision tasks are already blocked but their leases may not be released",
            );
            return;
        }
    };
    for execution in &executions {
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            chain_root_id,
            "merge poller: stopping revision execution — parent PR merged",
        );
        // Release the pane and cube workspace lease without altering
        // execution status (force_release does not change status).
        handler.force_release(&execution.id).await;
        // Now mark the execution terminal so the dispatcher won't try to
        // re-schedule it.  `cancel_execution` resets task status to `todo`
        // only when it's currently `active`; since the task is already
        // `blocked` (set in the DB transaction), that guard won't fire.
        match work_db.cancel_execution(&execution.id) {
            Ok(_) => {
                outcome.revision_invalidated += 1;
            }
            Err(err) => {
                // The execution may have already moved to a terminal state
                // (raced with the worker finishing, or a prior sweep).
                // Log at debug — not a concern since the lease is released.
                tracing::debug!(
                    execution_id = %execution.id,
                    ?err,
                    "merge poller: cancel_execution failed for revision (may already be terminal)",
                );
            }
        }
    }
}

/// Stop the live worker execution for `work_item_id` after its task
/// auto-transitioned back to `in_review` because the engine detected its
/// PR's CI had gone green (`on_ci_resolved`).
///
/// The worker that was running the task has nothing useful left to do:
/// the task reaching Review means its job is done. In the observed bug
/// (issue #898) the worker sat in `waiting_for_input`, polling CI checks
/// for the very fix the engine had already observed as green, holding a
/// worker slot indefinitely. We force-stop it regardless of what it is
/// doing — cancel the execution row and release its cube lease + pane.
///
/// [`WorkerCompletionHandler::force_stop_execution`] only demotes a task
/// that is still `active`; since the task is now `in_review`, that guard
/// does not fire and the task stays in Review. Idempotent: a no-op when
/// no live execution exists or `completion_handler` is `None` (tests /
/// cold-path wiring).
async fn stop_worker_on_review_transition(
    work_db: &WorkDb,
    completion_handler: Option<&WorkerCompletionHandler>,
    work_item_id: &str,
    outcome: &mut SweepOutcome,
) {
    let Some(handler) = completion_handler else {
        return;
    };
    // `exclude_id = ""` matches no real execution, so this returns the
    // genuinely-live worker for the task (not a phantom terminal row left
    // by a re-dispatch storm — see `get_live_execution_for_work_item`).
    let execution = match work_db.get_live_execution_for_work_item(work_item_id, "") {
        Ok(Some(exec)) => exec,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                ?err,
                "merge poller: failed to look up live execution to stop after Review transition; \
                 task is in_review but a worker may still be holding a slot",
            );
            return;
        }
    };
    tracing::info!(
        execution_id = %execution.id,
        work_item_id,
        "merge poller: stopping worker — task auto-transitioned to in_review (CI green)",
    );
    handler.force_stop_execution(&execution.id).await;
    outcome.worker_stopped_on_review += 1;
}

/// Re-run PR detection against an execution that the on-Stop hook
/// classified as having no PR but whose chore is still `active` (i.e.,
/// the worker stopped, the engine missed the PR-open transition, and
/// the chore is stuck in `active`). Delegates to
/// [`WorkerCompletionHandler::recheck_for_pr`], which transitions the
/// chore on `Fresh`/`Merged` and stays quiet on the no-PR / stale-PR
/// branches so the poller doesn't spam probes or awaiting-input events.
async fn sweep_pending_pr(
    handler: &WorkerCompletionHandler,
    execution_id: &str,
    outcome: &mut SweepOutcome,
) {
    match handler.recheck_for_pr(execution_id).await {
        StopOutcome::PrDetected { pr_url } => {
            outcome.pr_recheck_recovered += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: recovered missed PR-open for waiting_human worker",
            );
        }
        StopOutcome::PrMerged { pr_url } => {
            outcome.pr_recheck_recovered += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: recovered missed PR-open (PR already merged) for waiting_human worker",
            );
        }
        // Quiet branches — still no PR, transient detector failure,
        // or the execution moved on between list and recheck. Log at
        // info so a worker stuck in `waiting_human` with `pr_url=null`
        // leaves a breadcrumb on every sweep instead of failing
        // silently. Without this, the 2026-05-13 three-concurrent-
        // workers regression (where Worf/Crusher/Troi pushed real PRs
        // but the engine never bound them) had zero engine-log
        // evidence — the merge poller was running, the candidate query
        // listed the executions, but the recheck loop's silent return
        // hid the fact that `detect_pr` was returning Stale/None on
        // every pass.
        quiet @ (StopOutcome::AwaitingInput
        | StopOutcome::DetectorFailed
        | StopOutcome::StalePr { .. }
        | StopOutcome::EmptyDiffPr { .. }) => {
            outcome.pr_recheck_unresolved += 1;
            tracing::info!(
                execution_id,
                outcome = ?quiet,
                "merge poller: PR-detection recheck did not resolve this pass — \
                 worker still listed as waiting_human with no `pr_url`; \
                 will retry on next sweep (see `pr_detect:` log above for \
                 the underlying detector classification)",
            );
        }
        // These six are genuinely silent — the execution moved on
        // between `list` and `recheck` (raced with on-Stop / manual
        // intervention), hit a transient DB error, the running-
        // status gate (AI #6) skipped the fallback because the worker
        // is still alive, or the human flipped the
        // `detect_pr_cold_fallback` feature flag OFF (AI #5). No log
        // on these: they're not stuck-worker indicators.
        StopOutcome::AlreadyTerminal
        | StopOutcome::UnknownExecution
        | StopOutcome::SupersededInWorkspace
        | StopOutcome::NoWorkspace
        | StopOutcome::RunningNoStagedPr
        | StopOutcome::FallbackDisabledByFlag
        // `recheck_for_pr` never parks via the breaker (only the on-Stop
        // path nudges); covered here for exhaustiveness. SignalAlreadyCleared
        // is also only reachable via on-Stop, not recheck_for_pr.
        | StopOutcome::NudgeBreakerParked { .. }
        | StopOutcome::SignalAlreadyCleared { .. }
        | StopOutcome::DbError => {}
    }
}

/// Run late-PR detection against a terminal execution (abandoned /
/// completed / failed within the recent lookback window) whose task is
/// still `active` with no `pr_url`. Delegates to
/// [`WorkerCompletionHandler::recheck_for_pr_late`], which bypasses the
/// `AlreadyTerminal` gate and calls
/// [`WorkDb::bind_pr_to_active_task_from_terminal_execution`] directly
/// on a positive detection result.
async fn sweep_late_pr(
    handler: &WorkerCompletionHandler,
    candidate: &LatePrCandidate,
    outcome: &mut SweepOutcome,
) {
    match handler.recheck_for_pr_late(candidate).await {
        StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
            outcome.late_pr_recovered += 1;
            tracing::info!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                pr_url = %pr_url,
                "merge poller: late PR bound to active task (double-spawn recovery)",
            );
        }
        // No PR yet or stale — retry next sweep, no log spam.
        StopOutcome::AwaitingInput
        | StopOutcome::StalePr { .. }
        | StopOutcome::EmptyDiffPr { .. }
        | StopOutcome::DetectorFailed => {
            tracing::debug!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                "merge poller: late-PR recheck did not resolve — will retry next sweep",
            );
        }
        // Genuinely silent: execution/task moved on between list and recheck.
        StopOutcome::AlreadyTerminal
        | StopOutcome::UnknownExecution
        | StopOutcome::SupersededInWorkspace
        | StopOutcome::NoWorkspace
        | StopOutcome::RunningNoStagedPr
        | StopOutcome::FallbackDisabledByFlag
        // `recheck_for_pr_late` never parks via the breaker; covered for
        // exhaustiveness. SignalAlreadyCleared is only reachable via on-Stop.
        | StopOutcome::NudgeBreakerParked { .. }
        | StopOutcome::SignalAlreadyCleared { .. }
        | StopOutcome::DbError => {}
    }
}

/// Poll the PR's merge-queue timeline for `FAILED_CHECKS` dequeue
/// events and fire [`ci_watch::on_merge_queue_rebounce_detected`] for
/// any event whose `beforeCommit.oid` is not yet recorded in
/// `ci_remediations`. Best-effort: a failed GraphQL call is logged at
/// debug and skipped; the next sweep will retry.
async fn check_merge_queue_rebounce(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    let events = fetch_merge_queue_dequeue_events(&candidate.pr_url).await;
    if events.is_empty() {
        return;
    }
    for event in &events {
        let Some(before_commit_sha) = event.before_commit_oid.as_deref() else {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: FAILED_CHECKS dequeue event has no beforeCommit.oid; skipping",
            );
            continue;
        };
        if ci_watch::on_merge_queue_rebounce_detected(
            work_db,
            publisher,
            candidate,
            None, // head_ref_name not available without a probe round-trip
            None, // head_ref_oid not needed for rebounce (before_commit_sha is the key)
            before_commit_sha,
            &[], // labels not available here; opt-out check uses product flag only
        )
        .await
        {
            outcome.merge_queue_rebounced += 1;
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                before_commit_sha,
                "merge poller: merge-queue FAILED_CHECKS rebounce → blocked: ci_failure",
            );
        }
    }
}

async fn sweep_one(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&WorkerCompletionHandler>,
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    let probe_result = match probe.probe(&candidate.pr_url).await {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: probe failed; will retry next pass",
            );
            return;
        }
    };
    match &probe_result.state {
        PrLifecycleState::Merged => {
            if mark_merged(work_db, publisher, completion_handler, candidate, &probe_result).await {
                outcome.merged += 1;
                // Clean up any pending/running ci_remediations rows and emit
                // CiFailureCleared so the macOS kanban clears the "ci failing"
                // badge. Without this, a task that was blocked on CI when its
                // PR merged leaves a pending row that causes the badge to
                // reappear on every app restart / list-refresh.
                ci_watch::on_pr_merged(work_db, publisher, candidate).await;
            }
            // Invalidate any in-flight revision executions whose parent
            // just merged.  `block_pending_revisions_on_parent_close`
            // already ran inside `mark_chore_pr_merged`'s transaction;
            // here we force-release their cube leases and mark them
            // terminal so the scheduler doesn't try to redispatch.
            stop_active_revision_executions(
                work_db,
                completion_handler,
                &candidate.work_item_id,
                outcome,
            )
            .await;
        }
        PrLifecycleState::Open(open) => {
            // Design §Q1: conflict pre-empts CI — the conflict-resolver
            // owns the slot first, and CI will be re-evaluated against
            // the new base once the rebase pushes. Both clean drives
            // every retire path (conflict + CI), each gated by its own
            // WHERE guard so an irrelevant retire is a cheap no-op.
            let mergeability = open.mergeability;
            let ci = &open.ci;
            match mergeability {
                OpenPrMergeability::Conflict => {
                    // Phase 3 cutover: the conflict producer creates an
                    // engine-triggered revision via the shared
                    // `create_revision` gate (R4 reuse). We are inside the
                    // `Open` arm with `mergeability = Conflict`, so the PR is
                    // known-open; feed that observation to the gate via a
                    // static checker rather than a redundant `gh pr view`.
                    if conflict_watch::on_conflict_detected(
                        work_db,
                        publisher,
                        &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
                        candidate,
                        &probe_result,
                    )
                    .await
                    {
                        outcome.conflict_flagged += 1;
                    }
                }
                OpenPrMergeability::Clean => {
                    // Polymorphic clear dispatch (design §Q5 Phase 10 #31):
                    // walk the `task_blocked_signals` side table and ask
                    // each active reason's retire path to act if its
                    // probe condition holds. Each per-reason handler is
                    // still idempotent on its own (WHERE-guarded), so
                    // this is purely a refactor of the dispatch from
                    // "call every retire path unconditionally" to "call
                    // only the retire paths whose signals are still
                    // observed as active." The detect side stays where
                    // it is — detection is signal-specific (a `Failing`
                    // CI status can't fire the conflict watcher) and
                    // doesn't need the side-table read.
                    maybe_clear_blocked(
                        work_db,
                        publisher,
                        cube_client,
                        completion_handler,
                        candidate,
                        &probe_result.labels,
                        ci,
                        outcome,
                    )
                    .await;
                    // CI-side detect: a `Failing` rollup still needs
                    // its own fan-out regardless of what the side-table
                    // says, because the chore is currently `in_review`
                    // (no signal in the table yet) on the first failure.
                    if let OpenPrCiStatus::Failing { failures } = ci {
                        // Phase 4 cutover: the `fix`-kind CI producer creates
                        // an engine-triggered revision via the shared
                        // `create_revision` gate (R4 reuse). We are inside the
                        // `Open` arm with `mergeability = Clean`, so the PR is
                        // known-open; feed that observation to the gate via a
                        // static checker rather than a redundant `gh pr view`.
                        if ci_watch::on_ci_failure_detected(
                            work_db,
                            publisher,
                            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
                            candidate,
                            &probe_result,
                            failures,
                        )
                        .await
                        {
                            outcome.ci_flagged += 1;
                        }
                    }
                    // `InFlight` is the explicit "don't act yet" leaf
                    // for CI; the clear dispatch above already declined
                    // because `should_clear_ci` requires Clean. The
                    // never-starts soft alert (Phase 12 #39) tracks
                    // how long the same head sha has been sitting in
                    // InFlight and emits a warn at 30m / alert at 2h.
                    if matches!(ci, OpenPrCiStatus::InFlight) {
                        // Issue #901: a newer in-progress run supersedes
                        // an older failing result. The polymorphic clear
                        // dispatch above only retires on `Clean`, so a
                        // chore still parked in `blocked: ci_failure` from
                        // the previous run keeps its stale "ci failing"
                        // badge even though CI is now re-running. Clear it
                        // here so the card shows a single coherent
                        // "in progress" state rather than asserting a
                        // failure that is actively being re-evaluated.
                        if ci_watch::on_ci_in_flight_supersedes_failure(
                            work_db,
                            publisher,
                            candidate,
                            &probe_result.labels,
                        )
                        .await
                        {
                            outcome.ci_cleared += 1;
                        }
                        ci_watch::on_ci_in_flight(
                            work_db,
                            publisher,
                            candidate,
                            &probe_result,
                        )
                        .await;
                    }
                }
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
    // For every open (or just-probed) PR, persist the CI + review poll
    // state so the macOS kanban can render indicators with tooltips.
    // We do this unconditionally after the lifecycle routing above so
    // the columns stay fresh even when no status transition fired.
    // Merged / closed-unmerged probes are skipped — the row will
    // transition away from `in_review` and the indicators become moot.
    if matches!(probe_result.state, PrLifecycleState::Open(_)) {
        update_pr_poll_state(work_db, publisher, candidate, &probe_result).await;
    }
}

/// Polymorphic retire dispatch (design §Q5 / Phase 10 #31).
///
/// The merge poller's `Clean`-mergeability branch used to call every
/// per-signal retire path unconditionally (conflict-watch on_resolved
/// and ci-watch on_ci_resolved, in sequence). That worked because each
/// retire path was already WHERE-guarded against its own row state, so
/// running it against a chore that wasn't blocked on that reason was a
/// cheap no-op.
///
/// With the `task_blocked_signals` side table in place, we can do
/// better: read the active signal set first, and dispatch only to the
/// retire paths whose signals are still observed. Same end state, but:
///
///   - the dispatch is now self-documenting — adding a new
///     `blocked_reason` (review_feedback, dependency, …) becomes a
///     single match arm here rather than a new unconditional `await`
///     bolted onto the sweep;
///   - failure to add a per-reason `should_clear` arm becomes loud
///     (`_ => false` falls through with a warn), instead of silently
///     never clearing the signal;
///   - the per-signal probe condition is centralised, so the
///     `merge_conflict ⇒ Clean mergeability` and `ci_failure ⇒ Clean
///     ci` couplings live in one place that the design's snippet maps
///     to directly.
///
/// A read of the side table when there are no active signals is one
/// `SELECT … WHERE cleared_at IS NULL` returning zero rows; cheaper
/// than the unconditional UPDATEs the old dispatch always sent.
async fn maybe_clear_blocked(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&WorkerCompletionHandler>,
    candidate: &PendingMergeCheck,
    labels: &[String],
    ci: &OpenPrCiStatus,
    outcome: &mut SweepOutcome,
) {
    let signals = match work_db.active_blocked_signals(&candidate.work_item_id) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to read active blocked signals; skipping clear dispatch",
            );
            return;
        }
    };
    // Drift guard (T230): if `task_blocked_signals` is empty but the task
    // still has a non-null `blocked_reason`, the signals table and the
    // scalar got out of sync (e.g. the polymorphic-clear path cleared the
    // signal row before the parent task was cleared). Fall back to the
    // `blocked_reason` scalar so the retire path can still fire on a Clean
    // probe, preventing the task from being stuck blocked indefinitely.
    let signals = if signals.is_empty() {
        match work_db.task_blocked_reason(&candidate.work_item_id) {
            Ok(Some(reason)) => {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    %reason,
                    "merge poller: task_blocked_signals empty but blocked_reason set; using blocked_reason as fallback",
                );
                vec![boss_protocol::BlockedSignal {
                    work_item_id: candidate.work_item_id.clone(),
                    reason,
                    attempt_id: None,
                    created_at: String::new(),
                    cleared_at: None,
                }]
            }
            Ok(None) => return, // task not blocked or no reason — nothing to do
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "merge poller: failed to read blocked_reason for drift fallback; skipping clear dispatch",
                );
                return;
            }
        }
    } else {
        signals
    };

    // Mergeability is `Clean` at the caller (we're inside the Clean
    // arm of `sweep_one`), so the merge-conflict probe condition is
    // trivially true. CI's probe condition is `OpenPrCiStatus::Clean`
    // — `InFlight` and `Failing` decline to retire.
    let ci_clean = matches!(ci, OpenPrCiStatus::Clean);

    for signal in signals {
        match signal.reason.as_str() {
            "merge_conflict" => {
                if conflict_watch::on_resolved(
                    work_db,
                    publisher,
                    cube_client,
                    candidate,
                    labels,
                )
                .await
                {
                    outcome.conflict_cleared += 1;
                }
            }
            "ci_failure" | "ci_failure_exhausted" => {
                if !ci_clean {
                    continue;
                }
                if ci_watch::on_ci_resolved(work_db, publisher, candidate, labels).await {
                    outcome.ci_cleared += 1;
                    // The task just auto-transitioned back to `in_review`
                    // because its PR's CI went green. Stop the worker that
                    // was running it — it has nothing useful left to do (it
                    // is typically still polling CI for the very fix the
                    // engine already observed) and otherwise holds its slot
                    // indefinitely (issue #898).
                    stop_worker_on_review_transition(
                        work_db,
                        completion_handler,
                        &candidate.work_item_id,
                        outcome,
                    )
                    .await;
                }
            }
            other => {
                // Unknown / future blocked_reason values
                // (`review_feedback`, `dependency`, …) — those flows
                // own their own retire paths. We log once at debug so
                // an unwired reason doesn't silently leak past the
                // sweep, but don't treat the situation as an error.
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    reason = other,
                    "merge poller: no retire-path arm for blocked_reason; leaving for owning flow",
                );
            }
        }
    }
}

/// Derive the `ci_required_state` string from a probe's CI status.
fn ci_state_str(ci: &OpenPrCiStatus) -> &'static str {
    match ci {
        OpenPrCiStatus::Clean => "success",
        OpenPrCiStatus::InFlight => "in_progress",
        OpenPrCiStatus::Failing { .. } => "fail",
    }
}

/// Build a compact JSON detail blob for failing CI checks (list of
/// `{"name": "...", "conclusion": "..."}` objects). Returns `None`
/// when the check list is empty so we don't write `"[]"` to the DB.
fn ci_detail_json(ci: &OpenPrCiStatus) -> Option<String> {
    let OpenPrCiStatus::Failing { failures } = ci else {
        return None;
    };
    if failures.is_empty() {
        return None;
    }
    let items: Vec<serde_json::Value> = failures
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "conclusion": f.conclusion,
            })
        })
        .collect();
    serde_json::to_string(&items).ok()
}

/// Build a compact JSON detail blob for reviewer logins. Returns `None`
/// when the list is empty.
fn review_detail_json(reviewers: &[String]) -> Option<String> {
    if reviewers.is_empty() {
        return None;
    }
    serde_json::to_string(reviewers).ok()
}

/// Derive the `merge_queue_state` DB string from a probe's merge-queue flag.
/// Returns `Some("queued")` when in queue, `None` when not (NULL in DB).
fn merge_queue_state_str(in_merge_queue: bool) -> Option<&'static str> {
    if in_merge_queue { Some("queued") } else { None }
}

/// Persist CI + review + merge-queue poll state and emit a change event
/// when any field flips value. Called from `sweep_one` for every open PR and
/// from `completion.rs` after the on-transition initial CI fetch.
pub(crate) async fn update_pr_poll_state(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) {
    let PrLifecycleState::Open(open) = &probe.state else {
        return;
    };

    let ci_state = ci_state_str(&open.ci);
    let review_state = probe.review.as_db_str();
    let ci_detail = ci_detail_json(&open.ci);
    let review_detail = review_detail_json(probe.review.reviewers());
    let merge_queue_state = merge_queue_state_str(probe.in_merge_queue);

    match work_db.update_task_pr_poll_state(
        &candidate.work_item_id,
        ci_state,
        review_state,
        ci_detail.as_deref(),
        review_detail.as_deref(),
        merge_queue_state,
    ) {
        Ok(true) => {
            // State changed — emit event so the macOS kanban refreshes the
            // card's CI / review / merging indicators within the poll interval.
            publisher
                .publish_work_item_changed(
                    &candidate.product_id,
                    &candidate.work_item_id,
                    "pr_poll_state_updated",
                )
                .await;
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                ci_state,
                review_state,
                in_merge_queue = probe.in_merge_queue,
                "merge poller: PR poll state changed",
            );
        }
        Ok(false) => {
            // No state change (or row not found / deleted) — skip event.
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to update PR poll state",
            );
        }
    }
}

async fn mark_merged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    _completion_handler: Option<&WorkerCompletionHandler>,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
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
    // Kick the scheduler so any auto-unblocked dependents (whose
    // executions were just promoted to `ready` by the dep cascade)
    // are dispatched promptly rather than waiting for the next
    // external event or reconciler tick.
    publisher.kick_scheduler();
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        "merge poller: PR merged; work item moved to done",
    );
    // Auto-populate the project's design-doc pointer on merge for
    // `kind=design` tasks. Errors are logged inside the detector.
    if updated.kind == "design" {
        if let Some(ref project_id) = updated.project_id {
            design_detector::on_design_pr_merged(
                work_db,
                &updated.id,
                &candidate.product_id,
                project_id,
                &candidate.pr_url,
                probe.base_ref_name.as_deref(),
            )
            .await;
        }
    }
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
/// Extract the PR number from a GitHub PR URL.
///
/// Handles common URL shapes:
/// - `https://github.com/owner/repo/pull/123`
/// - `https://github.com/owner/repo/pull/123/files`
/// - `https://github.com/owner/repo/pull/123?foo=1`
///
/// Returns `None` when the URL doesn't contain `/pull/<digits>`.
pub(crate) fn parse_pr_number(pr_url: &str) -> Option<i64> {
    let stripped = pr_url.split('?').next().unwrap_or(pr_url);
    let stripped = stripped.split('#').next().unwrap_or(stripped);
    let tail = stripped.rsplit_once("/pull/")?.1;
    let n = tail.split(|c: char| !c.is_ascii_digit()).next()?;
    n.parse::<i64>().ok()
}

/// merged or developed a conflict while the engine was offline gets
/// reconciled on boot. The sweep runs inside the spawned task so
/// engine startup isn't blocked on `gh`; subsequent passes are
/// gated behind `interval`.
///
/// `kick` is a shared [`Notify`] the caller can fire (via
/// [`Notify::notify_one`]) to request an immediate out-of-band pass.
/// Kicks received within the 15 s quiesce window after the most
/// recent pass are silently dropped — the periodic tick will pick up
/// the change soon enough and rapid window-toggle events don't result
/// in repeated GitHub API calls.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    probe: Arc<dyn MergeProbe>,
    publisher: Arc<dyn ExecutionPublisher>,
    cube_client: Arc<dyn CubeClient>,
    completion_handler: Arc<WorkerCompletionHandler>,
    interval: Duration,
    metrics: Arc<Registry>,
    kick: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let quiesce_window = Duration::from_secs(15);
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                probe.as_ref(),
                publisher.as_ref(),
                Some(cube_client.as_ref()),
                Some(completion_handler.as_ref()),
            )
            .await;
            let last_run_at = Instant::now();
            MERGED.inc_by(&metrics, outcome.merged as u64);
            CONFLICT_FLAGGED.inc_by(&metrics, outcome.conflict_flagged as u64);
            CONFLICT_CLEARED.inc_by(&metrics, outcome.conflict_cleared as u64);
            PR_RECHECK_RECOVERED.inc_by(&metrics, outcome.pr_recheck_recovered as u64);
            PR_RECHECK_UNRESOLVED.inc_by(&metrics, outcome.pr_recheck_unresolved as u64);
            MERGE_QUEUE_REBOUNCED.inc_by(&metrics, outcome.merge_queue_rebounced as u64);
            LATE_PR_RECOVERED.inc_by(&metrics, outcome.late_pr_recovered as u64);
            REVISION_INVALIDATED.inc_by(&metrics, outcome.revision_invalidated as u64);
            WORKER_STOPPED_ON_REVIEW
                .inc_by(&metrics, outcome.worker_stopped_on_review as u64);
            if outcome.total_transitions() > 0 || outcome.pr_recheck_unresolved > 0 {
                tracing::info!(
                    merged = outcome.merged,
                    conflict_flagged = outcome.conflict_flagged,
                    conflict_cleared = outcome.conflict_cleared,
                    ci_flagged = outcome.ci_flagged,
                    ci_cleared = outcome.ci_cleared,
                    pr_recheck_recovered = outcome.pr_recheck_recovered,
                    pr_recheck_unresolved = outcome.pr_recheck_unresolved,
                    merge_queue_rebounced = outcome.merge_queue_rebounced,
                    late_pr_recovered = outcome.late_pr_recovered,
                    revision_invalidated = outcome.revision_invalidated,
                    worker_stopped_on_review = outcome.worker_stopped_on_review,
                    "merge poller: sweep transitions",
                );
            }

            // Wait for either the periodic interval or an activation kick.
            // Kicks received within the quiesce window are silently absorbed
            // — the inner loop keeps listening so the first kick that arrives
            // after the window has elapsed will trigger a pass immediately.
            'wait: loop {
                let elapsed = last_run_at.elapsed();
                let remaining_interval = interval.saturating_sub(elapsed);
                tokio::select! {
                    _ = tokio::time::sleep(remaining_interval) => {
                        break 'wait;
                    }
                    _ = kick.notified() => {
                        let since_last = last_run_at.elapsed();
                        if since_last >= quiesce_window {
                            tracing::debug!(
                                since_last_ms = since_last.as_millis(),
                                "merge poller: activation kick → immediate sweep",
                            );
                            break 'wait;
                        }
                        tracing::debug!(
                            since_last_ms = since_last.as_millis(),
                            quiesce_ms = quiesce_window.as_millis(),
                            "merge poller: kick within quiesce window, absorbing",
                        );
                        // continue listening; periodic sleep arm will eventually fire
                    }
                }
            }
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
    use crate::completion::{PrDetector, PrStatus, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser};
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus, ExecutionPublisher,
    };
    use crate::work::{
        ConflictResolutionInsertInput, CreateChoreInput, CreateExecutionInput, CreateProductInput,
        CreateProjectInput, CreateTaskInput, WorkDb, WorkItem, WorkItemPatch,
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
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: PrReviewState::Unknown,
                    in_merge_queue: false,
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
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: labels.iter().map(|s| (*s).to_owned()).collect(),
                    review: PrReviewState::Unknown,
                    in_merge_queue: false,
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
                    state: PrLifecycleState::Open(OpenPrStatus::clean()),
                    base_ref_oid: None,
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: PrReviewState::Unknown,
                    in_merge_queue: false,
                }),
            }
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        work_events: Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingPublisher {
        /// Events filtered to exclude poll-state housekeeping events
        /// (`pr_poll_state_updated`) so lifecycle-focused assertions don't
        /// have to account for the background sweep's bookkeeping writes.
        async fn lifecycle_reasons(&self) -> Vec<String> {
            self.work_events
                .lock()
                .await
                .iter()
                .filter(|(_, _, reason)| reason != "pr_poll_state_updated")
                .map(|(_, _, reason)| reason.clone())
                .collect()
        }
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: format!("Project-{name}"),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
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
                force_duplicate: false,
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
                force_duplicate: false,
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

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.merged, 0);
        assert_eq!(outcome.conflict_flagged, 0);
        // No `blocked: merge_conflict` row in the corpus, so the clean
        // signal hits nothing on the resolve side either.
        assert_eq!(outcome.conflict_cleared, 0);
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        // Only poll-state housekeeping events are allowed; no lifecycle flip.
        assert!(publisher.lifecycle_reasons().await.is_empty());
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
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.total_transitions(), 0);
        match db.get_work_item(&project_task_id).unwrap() {
            WorkItem::Task(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected project_task, got {other:?}"),
        }
        assert!(publisher.lifecycle_reasons().await.is_empty());
    }

    #[tokio::test]
    async fn empty_corpus_is_skipped() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        // No chores in review at all → no work, no errors, no events.
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.total_transitions(), 0);
        assert!(publisher.lifecycle_reasons().await.is_empty());
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome2.total_transitions(), 0);

        // Pass 3: probe flips to Clean; the blocked-conflict slice
        // picks the row up and clears it back to in_review.
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome4.total_transitions(), 0);

        // Event trail: blocked → resolved, plus the noop-passes
        // emitted nothing (poll-state events are excluded).
        let reasons: Vec<String> = publisher
            .work_events
            .lock()
            .await
            .iter()
            .filter(|(p, w, r)| p == &product && w == &chore && r != "pr_poll_state_updated")
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
        async fn list_repos(
            &self,
        ) -> Result<Vec<crate::coordinator::CubeRepoSummary>> {
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
            None,
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome = run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
            None,
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

    /// Helper to build a probe with CI failures + a head sha.
    fn probe_ci_failing(pr: &str, head_sha: &str) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr.to_owned(),
            state: PrLifecycleState::Open(OpenPrStatus::ci_failing(vec![RequiredCheckFailure {
                name: "ci/test".into(),
                conclusion: "FAILURE".into(),
                target_url: "".into(),
                provider: CiProvider::Other,
                provider_job_id: None,
            }])),
            base_ref_oid: Some("base-1".into()),
            head_ref_oid: Some(head_sha.to_owned()),
            head_ref_name: None,
            base_ref_name: None,
            labels: Vec::new(),
            review: PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    fn probe_ci_clean(pr: &str, head_sha: &str) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr.to_owned(),
            state: PrLifecycleState::Open(OpenPrStatus::clean()),
            base_ref_oid: Some("base-1".into()),
            head_ref_oid: Some(head_sha.to_owned()),
            head_ref_name: None,
            base_ref_name: None,
            labels: Vec::new(),
            review: PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    #[tokio::test]
    async fn sweep_drives_full_ci_failure_cycle() {
        // Phase 8 #22 acceptance: end-to-end through `run_one_pass`.
        // Pass 1: probe says CI failing → flip to blocked: ci_failure.
        // Pass 2: same probe (idempotent) → no transition.
        // Pass 3: probe flips to CI clean (after the worker pushed) →
        // retire path runs through the blocked_ci slice.
        // Pass 4: same retire (idempotent) → no transition.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/700";
        let (product, chore) = make_chore_in_review(&db, "Ccycle-ci", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1.
        probe
            .states
            .lock()
            .unwrap()
            .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.ci_flagged, 1, "first sweep must flip to ci_failure");
        assert_eq!(outcome.conflict_flagged, 0);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "blocked");
                assert_eq!(t.blocked_reason.as_deref(), Some("ci_failure"));
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Pass 2: probe still reports the same failure.
        let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(
            outcome2.total_transitions(),
            0,
            "idempotent re-probe must not re-fire",
        );

        // Pass 3: CI is clean. The blocked_ci slice picks the row up.
        probe
            .states
            .lock()
            .unwrap()
            .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
        let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome3.ci_cleared, 1, "next clean probe must retire");
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Pass 4: idempotent retire.
        let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome4.total_transitions(), 0);

        // Event trail: blocked → resolved (poll-state events excluded).
        let reasons: Vec<String> = publisher
            .work_events
            .lock()
            .await
            .iter()
            .filter(|(p, w, r)| p == &product && w == &chore && r != "pr_poll_state_updated")
            .map(|(_, _, r)| r.clone())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "blocked_ci_failure".to_owned(),
                "ci_failure_resolved".to_owned(),
            ],
        );
    }

    #[tokio::test]
    async fn list_chores_blocked_on_ci_failure_filters_correctly() {
        // Phase 8 #23 acceptance: the query returns only rows in
        // `blocked: ci_failure` or `ci_failure_exhausted` with a
        // `pr_url`, and excludes everything else (in_review,
        // blocked-on-other-reasons, soft-deleted, no-pr).
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr_ci = "https://github.com/foo/bar/pull/800";
        let pr_exh = "https://github.com/foo/bar/pull/801";
        let pr_mc = "https://github.com/foo/bar/pull/802";
        let pr_ir = "https://github.com/foo/bar/pull/803";

        let (_p_ci, ci_chore) = make_chore_in_review(&db, "C-ci", pr_ci);
        let (_p_exh, exh_chore) = make_chore_in_review(&db, "C-exh", pr_exh);
        let (_p_mc, mc_chore) = make_chore_in_review(&db, "C-mc", pr_mc);
        let (_p_ir, _ir_chore) = make_chore_in_review(&db, "C-ir", pr_ir);

        db.mark_chore_blocked_ci_failure(&ci_chore, pr_ci, None).unwrap();
        db.mark_chore_blocked_ci_failure_exhausted(&exh_chore, pr_exh).unwrap();
        db.mark_chore_blocked_merge_conflict(&mc_chore, pr_mc).unwrap();

        let listed = db.list_chores_blocked_on_ci_failure().unwrap();
        let ids: std::collections::HashSet<String> =
            listed.iter().map(|c| c.work_item_id.clone()).collect();
        assert!(
            ids.contains(&ci_chore),
            "ci_failure row must be listed; got {ids:?}",
        );
        assert!(
            ids.contains(&exh_chore),
            "ci_failure_exhausted row must be listed; got {ids:?}",
        );
        assert!(
            !ids.contains(&mc_chore),
            "merge_conflict row must NOT be in the CI list; got {ids:?}",
        );
        // The in_review row stays out (it doesn't satisfy
        // `status='blocked'`).
        assert_eq!(
            listed.len(),
            2,
            "exactly two CI-blocked rows should be returned",
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "blocked"),
            other => panic!("expected chore, got {other:?}"),
        }

        // Second pass: GitHub reports MERGED.
        probe.set(pr, PrLifecycleState::Merged);
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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

    /// Helper to build a `gh pr view --json …` JSON document for the
    /// parser-matrix tests. Defaults give an OPEN mergeable PR with no
    /// labels and no rollup; per-test overrides re-shape specific fields.
    fn json_doc(
        state: &str,
        merged_at: &str,
        mergeable: &str,
        merge_state_status: &str,
        base_ref_oid: &str,
        head_ref_oid: &str,
        labels: &[&str],
        rollup: serde_json::Value,
    ) -> String {
        let labels_json: Vec<serde_json::Value> = labels
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::json!({
            "state": state,
            "mergedAt": merged_at,
            "closedAt": "",
            "mergeable": mergeable,
            "mergeStateStatus": merge_state_status,
            "baseRefOid": base_ref_oid,
            "headRefOid": head_ref_oid,
            "labels": labels_json,
            "statusCheckRollup": rollup,
        })
        .to_string()
    }

    /// Mapping table for the parser's `(raw_state × mergeable ×
    /// mergeStateStatus)` rules. The truth table here mirrors the
    /// design doc's Q1 classification rules and guards against future
    /// tweaks rewriting them silently.
    #[test]
    fn parse_probe_covers_state_mergeable_status_matrix() {
        struct Case {
            label: &'static str,
            state: &'static str,
            merged_at: &'static str,
            mergeable: &'static str,
            merge_state_status: &'static str,
            base_ref_oid: &'static str,
            expect: PrLifecycleState,
            expect_base: Option<&'static str>,
        }
        let cases = [
            Case {
                label: "MERGED carries through even if mergeable is empty",
                state: "MERGED",
                merged_at: "2026-05-09T12:00:00Z",
                mergeable: "",
                merge_state_status: "",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Merged,
                expect_base: Some("abc"),
            },
            Case {
                label: "non-empty mergedAt overrides state=OPEN (edge: GH lag)",
                state: "OPEN",
                merged_at: "2026-05-09T12:00:00Z",
                mergeable: "MERGEABLE",
                merge_state_status: "CLEAN",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Merged,
                expect_base: Some("abc"),
            },
            Case {
                label: "CLOSED without merged falls to ClosedUnmerged",
                state: "CLOSED",
                merged_at: "",
                mergeable: "",
                merge_state_status: "",
                base_ref_oid: "abc",
                expect: PrLifecycleState::ClosedUnmerged,
                expect_base: Some("abc"),
            },
            Case {
                label: "OPEN + MERGEABLE/CLEAN is Clean",
                state: "OPEN",
                merged_at: "",
                mergeable: "MERGEABLE",
                merge_state_status: "CLEAN",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: Some("abc"),
            },
            Case {
                label: "OPEN + CONFLICTING/DIRTY is Conflict",
                state: "OPEN",
                merged_at: "",
                mergeable: "CONFLICTING",
                merge_state_status: "DIRTY",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::conflict_only()),
                expect_base: Some("abc"),
            },
            Case {
                label: "CONFLICTING without DIRTY status falls to Clean (lag protection)",
                state: "OPEN",
                merged_at: "",
                mergeable: "CONFLICTING",
                merge_state_status: "UNKNOWN",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: Some("abc"),
            },
            Case {
                label: "DIRTY without CONFLICTING falls to Clean (lag protection)",
                state: "OPEN",
                merged_at: "",
                mergeable: "MERGEABLE",
                merge_state_status: "DIRTY",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: Some("abc"),
            },
            Case {
                label: "UNKNOWN mergeable is treated as Clean (transient post-base-move)",
                state: "OPEN",
                merged_at: "",
                mergeable: "UNKNOWN",
                merge_state_status: "UNKNOWN",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: Some("abc"),
            },
            Case {
                label: "BEHIND is mergeable; not a conflict",
                state: "OPEN",
                merged_at: "",
                mergeable: "MERGEABLE",
                merge_state_status: "BEHIND",
                base_ref_oid: "abc",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: Some("abc"),
            },
            Case {
                label: "empty base ref is None",
                state: "OPEN",
                merged_at: "",
                mergeable: "MERGEABLE",
                merge_state_status: "CLEAN",
                base_ref_oid: "",
                expect: PrLifecycleState::Open(OpenPrStatus::clean()),
                expect_base: None,
            },
        ];
        for case in cases {
            let body = json_doc(
                case.state,
                case.merged_at,
                case.mergeable,
                case.merge_state_status,
                case.base_ref_oid,
                "",
                &[],
                serde_json::json!([]),
            );
            let probe = parse_probe_json("https://example.test/pr/1", &body, None).unwrap();
            assert_eq!(
                probe.state, case.expect,
                "case `{}`: state mismatch (body: {:?})",
                case.label, body,
            );
            assert_eq!(
                probe.base_ref_oid.as_deref(),
                case.expect_base,
                "case `{}`: base_ref_oid mismatch",
                case.label,
            );
            assert!(
                probe.labels.is_empty(),
                "case `{}`: labels mismatch (none expected)",
                case.label,
            );
        }
    }

    /// Labels arrive as an array of `{name, …}` objects from gh. Empty
    /// stays empty; the conflict-watch opt-out uses these to honour
    /// the per-PR `boss/no-auto-rebase` label.
    #[test]
    fn parse_probe_parses_labels_column() {
        let body = json_doc(
            "OPEN",
            "",
            "MERGEABLE",
            "CLEAN",
            "abc",
            "",
            &["needs-review", "boss/no-auto-rebase"],
            serde_json::json!([]),
        );
        let probe = parse_probe_json("https://example.test/pr/2", &body, None).unwrap();
        assert_eq!(
            probe.labels,
            vec!["needs-review".to_owned(), "boss/no-auto-rebase".to_owned()],
        );

        let body_empty = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "abc", "", &[], serde_json::json!([]),
        );
        let probe_empty =
            parse_probe_json("https://example.test/pr/3", &body_empty, None).unwrap();
        assert!(probe_empty.labels.is_empty());
    }

    /// `(state × mergeability × ci-leaf-set × combined-state)` matrix for
    /// the CI predicate. Exercises the latest-leaf-per-name collapse, the
    /// required/not-required filter, the closed conclusion set from design
    /// §Q1 / Phase 8 #21, and the combined-commit-status fallback used to
    /// surface EXPECTED (not-yet-submitted) required checks.
    #[test]
    fn parse_probe_covers_ci_leaf_set_matrix() {
        struct Case {
            label: &'static str,
            rollup: serde_json::Value,
            /// Simulates the legacy commit-status combined state returned by
            /// `GET /repos/{owner}/{repo}/commits/{sha}/status`.
            combined_state: Option<&'static str>,
            expect_ci: OpenPrCiStatus,
        }
        let failing_check =
            |name: &'static str, conclusion: &'static str, target: &'static str| {
                serde_json::json!({
                    "name": name,
                    "status": "COMPLETED",
                    "conclusion": conclusion,
                    "targetUrl": target,
                    "isRequired": true,
                })
            };
        let success_check = |name: &'static str| {
            serde_json::json!({
                "name": name,
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
                "isRequired": true,
            })
        };
        let cases = [
            Case {
                label: "no rollup, no combined state → Clean (no CI configured)",
                rollup: serde_json::json!([]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "no rollup + combined pending → InFlight (EXPECTED checks not yet submitted)",
                rollup: serde_json::json!([]),
                combined_state: Some("pending"),
                expect_ci: OpenPrCiStatus::InFlight,
            },
            Case {
                label: "no rollup + combined success → Clean (no required checks)",
                rollup: serde_json::json!([]),
                combined_state: Some("success"),
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "no rollup + combined failure → InFlight (conservative; no check details yet)",
                rollup: serde_json::json!([]),
                combined_state: Some("failure"),
                expect_ci: OpenPrCiStatus::InFlight,
            },
            Case {
                label: "all required checks SUCCESS → Clean",
                rollup: serde_json::json!([success_check("ci/build"), success_check("ci/test")]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "one required check FAILURE → Failing",
                rollup: serde_json::json!([
                    success_check("ci/build"),
                    failing_check("ci/test", "FAILURE", ""),
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "ci/test".into(),
                        conclusion: "FAILURE".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "later leaf wins for the same name (re-run success masks earlier FAILURE)",
                rollup: serde_json::json!([
                    failing_check("ci/test", "FAILURE", ""),
                    success_check("ci/test"),
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "later leaf wins for the same name (re-run FAILURE masks earlier success)",
                rollup: serde_json::json!([
                    success_check("ci/test"),
                    failing_check("ci/test", "FAILURE", ""),
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "ci/test".into(),
                        conclusion: "FAILURE".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "non-required failing check is ignored",
                rollup: serde_json::json!([
                    {
                        "name": "third-party/lint",
                        "status": "COMPLETED",
                        "conclusion": "FAILURE",
                        "isRequired": false,
                    },
                    success_check("ci/test"),
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "required check IN_PROGRESS → InFlight (we wait)",
                rollup: serde_json::json!([
                    {
                        "name": "ci/test",
                        "status": "IN_PROGRESS",
                        "conclusion": serde_json::Value::Null,
                        "isRequired": true,
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::InFlight,
            },
            Case {
                label: "STARTUP_FAILURE counts as failure (engine pre-triages to retrigger)",
                rollup: serde_json::json!([failing_check("ci/build", "STARTUP_FAILURE", "")]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "ci/build".into(),
                        conclusion: "STARTUP_FAILURE".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "TIMED_OUT counts as failure",
                rollup: serde_json::json!([failing_check("ci/test", "TIMED_OUT", "")]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "ci/test".into(),
                        conclusion: "TIMED_OUT".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "NEUTRAL and SKIPPED are passes (don't gate merge)",
                rollup: serde_json::json!([
                    {
                        "name": "ci/changelog",
                        "status": "COMPLETED",
                        "conclusion": "NEUTRAL",
                        "isRequired": true,
                    },
                    {
                        "name": "ci/coverage",
                        "status": "COMPLETED",
                        "conclusion": "SKIPPED",
                        "isRequired": true,
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "mixed: failure + in-flight → Failing (we have a definitive signal)",
                rollup: serde_json::json!([
                    failing_check("ci/test", "FAILURE", ""),
                    {
                        "name": "ci/lint",
                        "status": "IN_PROGRESS",
                        "conclusion": serde_json::Value::Null,
                        "isRequired": true,
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "ci/test".into(),
                        conclusion: "FAILURE".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "Buildkite target URL → provider inferred",
                rollup: serde_json::json!([failing_check(
                    "buildkite/mono",
                    "FAILURE",
                    "https://buildkite.com/anthropic/mono/builds/42#01h-job-uuid",
                )]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "buildkite/mono".into(),
                        conclusion: "FAILURE".into(),
                        target_url:
                            "https://buildkite.com/anthropic/mono/builds/42#01h-job-uuid".into(),
                        provider: CiProvider::Buildkite,
                        provider_job_id: Some("01h-job-uuid".into()),
                    }],
                },
            },
            Case {
                label: "GitHub Actions target URL → provider inferred",
                rollup: serde_json::json!([failing_check(
                    "gha/build",
                    "FAILURE",
                    "https://github.com/anthropic/mono/actions/runs/12345/job/67890",
                )]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "gha/build".into(),
                        conclusion: "FAILURE".into(),
                        target_url:
                            "https://github.com/anthropic/mono/actions/runs/12345/job/67890".into(),
                        provider: CiProvider::GithubActions,
                        provider_job_id: Some("67890".into()),
                    }],
                },
            },
            // ---- StatusContext leaf shape (legacy commit-status API,
            // used by Buildkite and other CI integrations). These
            // leaves carry `context` + `state` and have NO `status` or
            // `conclusion` field. Pre-fix the parser silently classified
            // every StatusContext leaf as InFlight; the next four cases
            // pin the StatusContext code path so a future regression
            // shows up as a test failure rather than a stuck yellow
            // clock on every chore card.
            Case {
                label: "StatusContext: all SUCCESS → Clean (Buildkite-style rollup)",
                rollup: serde_json::json!([
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono",
                        "state": "SUCCESS",
                        "targetUrl": "https://buildkite.com/flunge/mono/builds/91",
                    },
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono/checks",
                        "state": "SUCCESS",
                        "targetUrl": "https://buildkite.com/flunge/mono/builds/91#abc",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "StatusContext: PENDING → InFlight",
                rollup: serde_json::json!([
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono",
                        "state": "PENDING",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::InFlight,
            },
            Case {
                label: "StatusContext: FAILURE → Failing",
                rollup: serde_json::json!([
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono",
                        "state": "FAILURE",
                        "targetUrl": "https://buildkite.com/flunge/mono/builds/91#019e",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "buildkite/mono".into(),
                        conclusion: "FAILURE".into(),
                        target_url: "https://buildkite.com/flunge/mono/builds/91#019e".into(),
                        provider: CiProvider::Buildkite,
                        provider_job_id: Some("019e".into()),
                    }],
                },
            },
            Case {
                label: "StatusContext: ERROR is a failure (legacy commit-status crash state)",
                rollup: serde_json::json!([
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono",
                        "state": "ERROR",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Failing {
                    failures: vec![RequiredCheckFailure {
                        name: "buildkite/mono".into(),
                        conclusion: "ERROR".into(),
                        target_url: "".into(),
                        provider: CiProvider::Other,
                        provider_job_id: None,
                    }],
                },
            },
            Case {
                label: "Mixed CheckRun + StatusContext, all green → Clean",
                rollup: serde_json::json!([
                    success_check("ci/build"),
                    {
                        "__typename": "StatusContext",
                        "context": "buildkite/mono",
                        "state": "SUCCESS",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "StatusContext: SUCCESS without __typename (defensive fallback)",
                rollup: serde_json::json!([
                    {
                        "context": "legacy/check",
                        "state": "SUCCESS",
                    },
                ]),
                combined_state: None,
                expect_ci: OpenPrCiStatus::Clean,
            },
        ];
        for case in cases {
            let body = json_doc(
                "OPEN", "", "MERGEABLE", "CLEAN", "abc", "head-1", &[], case.rollup.clone(),
            );
            let probe =
                parse_probe_json("https://example.test/pr/ci", &body, case.combined_state)
                    .unwrap();
            let actual_ci = match probe.state {
                PrLifecycleState::Open(OpenPrStatus { ci, .. }) => ci,
                other => panic!("case `{}`: expected Open, got {other:?}", case.label),
            };
            assert_eq!(
                actual_ci, case.expect_ci,
                "case `{}`: CI status mismatch",
                case.label,
            );
        }
    }

    /// GitHub's `commits/{sha}/status` endpoint returns `state:"pending"`
    /// for a commit with zero recorded statuses. Without filtering on
    /// `total_count` the empty-rollup PR card would render a stuck yellow
    /// "waiting for CI" icon for repos that have no checks configured at
    /// all. The helper must collapse that case to `None`, which the
    /// caller folds into `OpenPrCiStatus::Clean`.
    #[test]
    fn parse_combined_status_zero_total_count_returns_none() {
        let body = serde_json::json!({"state": "pending", "total_count": 0}).to_string();
        assert_eq!(parse_combined_status_response(&body), None);
    }

    #[test]
    fn parse_combined_status_surfaces_state_when_count_positive() {
        let cases = [
            ("pending", "pending"),
            ("PENDING", "pending"),
            ("success", "success"),
            ("failure", "failure"),
            ("error", "error"),
        ];
        for (input, expected) in cases {
            let body = serde_json::json!({"state": input, "total_count": 1}).to_string();
            assert_eq!(
                parse_combined_status_response(&body),
                Some(expected.to_string()),
                "state={input}",
            );
        }
    }

    #[test]
    fn parse_combined_status_handles_missing_or_empty_fields() {
        // Missing total_count defaults to 0 → treat as no checks.
        let no_count = serde_json::json!({"state": "pending"}).to_string();
        assert_eq!(parse_combined_status_response(&no_count), None);

        // Empty state with positive count → None (defensive).
        let empty_state = serde_json::json!({"state": "", "total_count": 2}).to_string();
        assert_eq!(parse_combined_status_response(&empty_state), None);

        // Malformed JSON → None.
        assert_eq!(parse_combined_status_response("not json"), None);
    }

    /// Conflict pre-empts CI in the joint state (design §Q1 dispatch
    /// table); the parser still surfaces both axes so callers can
    /// inspect either. The merge_poller sweep only acts on the conflict
    /// axis when both fire, but the probe doesn't lose data.
    #[test]
    fn parse_probe_surfaces_conflict_and_ci_failure_together() {
        let body = json_doc(
            "OPEN",
            "",
            "CONFLICTING",
            "DIRTY",
            "base-1",
            "head-1",
            &[],
            serde_json::json!([{
                "name": "ci/test",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "isRequired": true,
            }]),
        );
        let probe = parse_probe_json("https://example.test/pr/both", &body, None).unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(open.mergeability, OpenPrMergeability::Conflict);
        assert!(
            matches!(open.ci, OpenPrCiStatus::Failing { .. }),
            "ci must remain Failing alongside Conflict; got {:?}",
            open.ci,
        );
        assert_eq!(probe.head_ref_oid.as_deref(), Some("head-1"));
    }

    /// `mergeQueueEntry` field: non-null → `in_merge_queue = true`,
    /// null / absent → `in_merge_queue = false`.
    #[test]
    fn parse_probe_detects_merge_queue_entry() {
        // PR in merge queue — mergeQueueEntry is a non-null object.
        let body_in_queue = {
            let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
                "OPEN", "", "MERGEABLE", "CLEAN", "", "", &[], serde_json::json!([]),
            ))
            .unwrap();
            doc["mergeQueueEntry"] = serde_json::json!({"state": "QUEUED"});
            doc.to_string()
        };
        let probe = parse_probe_json("https://example.test/pr/mq1", &body_in_queue, None).unwrap();
        assert!(probe.in_merge_queue, "non-null mergeQueueEntry should set in_merge_queue");

        // PR not in merge queue — mergeQueueEntry is JSON null.
        let body_null = {
            let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
                "OPEN", "", "MERGEABLE", "CLEAN", "", "", &[], serde_json::json!([]),
            ))
            .unwrap();
            doc["mergeQueueEntry"] = serde_json::Value::Null;
            doc.to_string()
        };
        let probe_null =
            parse_probe_json("https://example.test/pr/mq2", &body_null, None).unwrap();
        assert!(!probe_null.in_merge_queue, "null mergeQueueEntry should clear in_merge_queue");

        // PR not in merge queue — mergeQueueEntry field absent entirely
        // (older gh versions or repos without queue enabled).
        let body_absent = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "", "", &[], serde_json::json!([]),
        );
        let probe_absent =
            parse_probe_json("https://example.test/pr/mq3", &body_absent, None).unwrap();
        assert!(
            !probe_absent.in_merge_queue,
            "absent mergeQueueEntry should clear in_merge_queue",
        );
    }

    /// Build a CheckRun rollup leaf with the given name + verdict shape.
    fn check_run(name: &str, status: &str, conclusion: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "isRequired": true,
        })
    }

    /// LinkedIn-org reclassification: a PR in `linkedin-multiproduct`
    /// with `Owner Approval` pending and no other failing check should
    /// surface as CI clean + review required, not CI in-flight. Without
    /// the reclassification at the aggregation layer the card reads
    /// "Required CI checks in progress" when the real situation is
    /// "waiting for owner review", which is what the issue asks to fix.
    #[test]
    fn owner_approval_pending_in_linkedin_org_routes_to_review() {
        let rollup = serde_json::json!([
            check_run("ci/build", "COMPLETED", "SUCCESS"),
            check_run("Owner Approval", "IN_PROGRESS", ""),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/linkedin-multiproduct/mono/pull/1",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(
            open.ci,
            OpenPrCiStatus::Clean,
            "Owner Approval pending must not contribute to CI status",
        );
        assert_eq!(
            probe.review,
            PrReviewState::Required,
            "Owner Approval pending must surface as review-required",
        );
    }

    /// Dominance rule: even when GitHub's `reviewDecision` reports
    /// `APPROVED` (the code-review side is satisfied), a pending
    /// `Owner Approval` check still gates merge and must show the
    /// PR as awaiting required review.
    #[test]
    fn owner_approval_pending_overrides_github_approved_decision() {
        let rollup = serde_json::json!([
            check_run("Owner Approval", "IN_PROGRESS", ""),
        ]);
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        ))
        .unwrap();
        doc["reviewDecision"] = serde_json::json!("APPROVED");
        doc["reviews"] = serde_json::json!([
            {"author": {"login": "alice"}, "state": "APPROVED"},
        ]);
        let probe = parse_probe_json(
            "https://github.com/linkedin-eng/foo/pull/2",
            &doc.to_string(),
            None,
        )
        .unwrap();
        assert_eq!(probe.review, PrReviewState::Required);
    }

    /// `ChangesRequested` is a stronger negative signal than a pending
    /// owner-approval check; preserve it rather than overriding to
    /// `Required` so the user still sees who blocked the PR.
    #[test]
    fn owner_approval_pending_preserves_changes_requested() {
        let rollup = serde_json::json!([
            check_run("Owner Approval", "IN_PROGRESS", ""),
        ]);
        let mut doc: serde_json::Value = serde_json::from_str(&json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        ))
        .unwrap();
        doc["reviewDecision"] = serde_json::json!("CHANGES_REQUESTED");
        doc["reviews"] = serde_json::json!([
            {"author": {"login": "bob"}, "state": "CHANGES_REQUESTED"},
        ]);
        let probe = parse_probe_json(
            "https://github.com/linkedin-multiproduct/mono/pull/3",
            &doc.to_string(),
            None,
        )
        .unwrap();
        assert_eq!(
            probe.review,
            PrReviewState::ChangesRequested { reviewers: vec!["bob".to_owned()] },
        );
    }

    /// Successful Owner Approval is a no-op for the review axis — the
    /// GitHub verdict (here `Unknown` since `reviewDecision` is unset)
    /// stands.
    #[test]
    fn owner_approval_success_does_not_override_review() {
        let rollup = serde_json::json!([
            check_run("Owner Approval", "COMPLETED", "SUCCESS"),
            check_run("ci/build", "COMPLETED", "SUCCESS"),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/linkedin-multiproduct/mono/pull/4",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(open.ci, OpenPrCiStatus::Clean);
        assert_eq!(probe.review, PrReviewState::Unknown);
    }

    /// Failed Owner Approval (ACL rejection) is reported as
    /// `ChangesRequested` with no reviewer identity, and is removed
    /// from the CI axis so the engine's CI-fix flow doesn't try to
    /// auto-remediate a human-approval refusal.
    #[test]
    fn owner_approval_failure_becomes_changes_requested() {
        let rollup = serde_json::json!([
            check_run("Owner Approval", "COMPLETED", "FAILURE"),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/linkedin-eng/foo/pull/5",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(
            open.ci,
            OpenPrCiStatus::Clean,
            "Owner Approval failure must not show as a CI failure",
        );
        assert_eq!(
            probe.review,
            PrReviewState::ChangesRequested { reviewers: Vec::new() },
        );
    }

    /// Outside the configured LinkedIn orgs, an `Owner Approval` check
    /// is left in the CI rollup and behaves like any other required
    /// check — this guards against the reclassification leaking into
    /// repos where the check doesn't have ACL semantics.
    #[test]
    fn owner_approval_in_other_org_stays_a_ci_check() {
        let rollup = serde_json::json!([
            check_run("Owner Approval", "IN_PROGRESS", ""),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/spinyfin/mono/pull/6",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(
            open.ci,
            OpenPrCiStatus::InFlight,
            "non-LinkedIn org: Owner Approval contributes to CI as normal",
        );
        assert_eq!(probe.review, PrReviewState::Unknown);
    }

    /// Org matching is case-insensitive on the URL owner segment;
    /// GitHub preserves casing for org slugs but the engine should
    /// tolerate drift in user-supplied URLs.
    #[test]
    fn linkedin_org_match_is_case_insensitive() {
        let rollup = serde_json::json!([
            check_run("owner approval", "IN_PROGRESS", ""),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/LinkedIn-Multiproduct/mono/pull/7",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(open.ci, OpenPrCiStatus::Clean);
        assert_eq!(probe.review, PrReviewState::Required);
    }

    /// A LinkedIn-org PR without an `Owner Approval` check at all
    /// (e.g. an older PR that predates the gate) is treated as having
    /// no review-signal verdict — both axes behave as normal.
    #[test]
    fn linkedin_org_without_owner_approval_is_unchanged() {
        let rollup = serde_json::json!([
            check_run("ci/build", "COMPLETED", "SUCCESS"),
        ]);
        let body = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "base-1", "head-1", &[], rollup,
        );
        let probe = parse_probe_json(
            "https://github.com/linkedin-multiproduct/mono/pull/8",
            &body,
            None,
        )
        .unwrap();
        let open = match probe.state {
            PrLifecycleState::Open(open) => open,
            other => panic!("expected Open, got {other:?}"),
        };
        assert_eq!(open.ci, OpenPrCiStatus::Clean);
        assert_eq!(probe.review, PrReviewState::Unknown);
    }

    #[test]
    fn owner_from_pr_url_extracts_owner_segment() {
        assert_eq!(
            super::owner_from_pr_url("https://github.com/linkedin-multiproduct/mono/pull/1"),
            Some("linkedin-multiproduct"),
        );
        assert_eq!(
            super::owner_from_pr_url("https://github.com/spinyfin/mono/pull/568"),
            Some("spinyfin"),
        );
        assert_eq!(super::owner_from_pr_url("not-a-url"), None);
    }

    #[test]
    fn repo_from_pr_url_extracts_owner_repo() {
        assert_eq!(
            super::repo_from_pr_url("https://github.com/spinyfin/mono/pull/568"),
            Some("spinyfin/mono"),
        );
        assert_eq!(
            super::repo_from_pr_url("https://github.com/owner/my-repo/pull/1"),
            Some("owner/my-repo"),
        );
        assert_eq!(super::repo_from_pr_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(super::repo_from_pr_url("not-a-url"), None);
    }

    #[test]
    fn pr_number_from_url_extracts_number() {
        assert_eq!(
            super::pr_number_from_url("https://github.com/spinyfin/mono/pull/568"),
            Some(568),
        );
        assert_eq!(
            super::pr_number_from_url("https://github.com/owner/my-repo/pull/1"),
            Some(1),
        );
        assert_eq!(super::pr_number_from_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(super::pr_number_from_url("not-a-url"), None);
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        // No prior probe activity — this is the very first sweep,
        // exactly what runs at engine startup.
        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
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
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            &["boss/no-auto-rebase"],
        );
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_flagged, 0);
        assert_eq!(outcome.total_transitions(), 0);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(publisher.lifecycle_reasons().await.is_empty());
    }

    /// Helper: seed a chore into `blocked: merge_conflict` with a pending

    /// T230 scenario integration test: worker B resolved against stale main
    /// SHA (already-succeeded crz), but PR is still CONFLICTING. The next
    /// merge-poller sweep must:
    ///   1. Detect the stale-base situation (succeeded crz + CONFLICTING PR).
    ///   2. Re-arm `task_blocked_signals`.
    ///   3. Dispatch a fresh crz against the new base SHA.
    ///   4. Leave all four state surfaces mutually consistent.
    #[tokio::test]
    async fn stale_base_succeeded_crz_rearmed_on_conflicting_pr() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/910";
        let (product, chore) = make_chore_in_review(&db, "C-t230", pr);

        // Simulate: conflict detected against old main SHA "sha-old".
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 910,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("sha-old".into()),
                head_sha_before: Some("sha-head-before".into()),
            })
            .unwrap()
            .expect("attempt insert must succeed");
        db.mark_conflict_resolution_running(&attempt.id, "lease-t230", "ws-t230", "worker-t230")
            .unwrap();

        // Worker B ran against the stale base and marked the crz succeeded.
        // (In the real scenario the task flip inside finalize_via_resolution_signal
        // missed due to blocked_attempt_id mismatch; here we reproduce the exact
        // wedged state: crz=succeeded, task=blocked:merge_conflict.)
        db.mark_conflict_resolution_succeeded(&attempt.id, Some("sha-head-after"))
            .unwrap();
        // Ensure task is still blocked (the primary path's WHERE guard missed).
        let task = match db.get_work_item(&chore).unwrap() {
            crate::work::WorkItem::Chore(t) => t,
            other => panic!("expected Chore, got {other:?}"),
        };
        assert_eq!(task.status, "blocked");
        assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));

        // Probe now reports CONFLICTING against the *new* main SHA "sha-new".
        let probe = StubProbe::new();
        probe.set_with_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            Some("sha-new"),
        );
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

        // The sweep must NOT count this as a new conflict_flagged
        // (the task didn't flip from in_review — it was already blocked).
        assert_eq!(outcome.conflict_flagged, 0, "no new flip expected");

        // A new crz must exist with base_sha_at_trigger = "sha-new".
        let crz_rows = db
            .list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap();
        let fresh_crz = crz_rows
            .iter()
            .find(|r| r.base_sha_at_trigger.as_deref() == Some("sha-new"))
            .unwrap_or_else(|| {
                panic!("expected a fresh crz with base_sha_at_trigger=sha-new; rows={crz_rows:?}")
            });
        assert_eq!(fresh_crz.status, "pending", "fresh crz must be pending");

        // Phase 3 cutover: the re-arm spawns an engine-triggered revision
        // (not a bespoke conflict_resolution execution) as the fix vehicle.
        // The fresh crz carries the reverse link to that revision.
        let revision_task_id = fresh_crz
            .revision_task_id
            .as_deref()
            .expect("fresh crz must carry a revision_task_id after the re-arm cutover");
        let revision = match db.get_work_item(revision_task_id).unwrap() {
            crate::work::WorkItem::Task(t) => t,
            other => panic!("expected revision task, got {other:?}"),
        };
        assert_eq!(revision.kind, "revision");
        assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
        assert!(
            revision.created_via.starts_with("merge-conflict:"),
            "revision created_via must carry merge-conflict provenance; got {}",
            revision.created_via,
        );

        // The dormant conflict_resolution dispatch must NOT fire post-cutover.
        let ready = db.list_ready_executions().unwrap();
        assert!(
            !ready
                .iter()
                .any(|e| e.work_item_id == chore && e.kind == "conflict_resolution"),
            "cutover must not create a conflict_resolution execution; got {ready:?}",
        );

        // The original crz must still be succeeded.
        let orig = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
        assert_eq!(orig.status, "succeeded");

        // task_blocked_signals must have an active merge_conflict row.
        let signals = db.active_blocked_signals(&chore).unwrap();
        assert!(
            signals.iter().any(|s| s.reason == "merge_conflict"),
            "merge_conflict signal must be active after re-arm; got {signals:?}",
        );

        // tasks.blocked_reason must still be merge_conflict.
        let task_after = match db.get_work_item(&chore).unwrap() {
            crate::work::WorkItem::Chore(t) => t,
            other => panic!("expected Chore, got {other:?}"),
        };
        assert_eq!(task_after.status, "blocked");
        assert_eq!(task_after.blocked_reason.as_deref(), Some("merge_conflict"));
    }

    /// Complement test: a `failed` crz must NOT be re-armed (churn guard
    /// and human own the retry). Verifies the stale-base path doesn't
    /// widen to swallow the churn guard's intention.
    #[tokio::test]
    async fn failed_crz_is_not_rearmed_on_conflicting_pr() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/911";
        let (product, chore) = make_chore_in_review(&db, "C-failed-norearm", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product,
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 911,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("sha-fail".into()),
                head_sha_before: None,
            })
            .unwrap()
            .expect("attempt insert must succeed");
        db.mark_conflict_resolution_failed(&attempt.id, "worker_died")
            .unwrap();

        let probe = StubProbe::new();
        probe.set_with_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            Some("sha-new"),
        );
        let publisher = Arc::new(RecordingPublisher::default());
        run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

        let ready = db.list_ready_executions().unwrap();
        assert!(
            ready.is_empty(),
            "failed crz must not be re-armed automatically; got {ready:?}",
        );
    }

    /// Drift-guard: when `task_blocked_signals` is empty but
    /// `blocked_reason = 'merge_conflict'` and the probe returns Clean,
    /// `maybe_clear_blocked` must still fire the retire path and flip the
    /// task back to `in_review`.
    #[tokio::test]
    async fn drift_guard_clears_blocked_task_when_signals_empty_but_pr_clean() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/912";
        let (product, chore) = make_chore_in_review(&db, "C-drift-clean", pr);

        // Put the task into blocked:merge_conflict (signals + reason both set).
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

        // Simulate the drift: clear the signal row manually without clearing
        // the blocked_reason on the tasks table.
        {
            let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
            conn.execute(
                "UPDATE task_blocked_signals SET cleared_at = '9999' WHERE work_item_id = ?1",
                [&chore],
            )
            .unwrap();
        }

        // Sanity: signal is now empty but blocked_reason is still set.
        assert!(db.active_blocked_signals(&chore).unwrap().is_empty());
        let task = match db.get_work_item(&chore).unwrap() {
            crate::work::WorkItem::Chore(t) => t,
            _ => panic!(),
        };
        assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));

        // Probe now returns Clean — the PR is mergeable.
        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

        // The drift guard must have fired the retire path.
        assert_eq!(
            outcome.conflict_cleared, 1,
            "drift guard must clear the blocked task when signals empty and PR clean",
        );

        // Task must be back in_review.
        let task_after = match db.get_work_item(&chore).unwrap() {
            crate::work::WorkItem::Chore(t) => t,
            _ => panic!(),
        };
        assert_eq!(task_after.status, "in_review");
        assert!(task_after.blocked_reason.is_none());

        // work_item_changed event must have fired.
        let events = publisher.work_events.lock().await;
        assert!(
            events.iter().any(|(pid, wid, r)| pid == &product && wid == &chore && r == "merge_conflict_resolved"),
            "expected merge_conflict_resolved event; got {events:?}",
        );
    }

    #[tokio::test]
    async fn activation_kick_quiesce_absorbs_rapid_repeats() {
        use tokio::time::timeout;

        let kick = Arc::new(Notify::new());
        let quiesce_window = Duration::from_millis(200); // short for tests
        let interval = Duration::from_secs(3600); // never fires

        // Simulate: last run just finished.
        let last_run_at = Instant::now();

        // Fire a kick immediately (well within the quiesce window).
        kick.notify_one();

        // The 'wait loop should absorb the kick and NOT break out within
        // a short window. We run one iteration of the select: if kick
        // fires and elapsed < quiesce_window, the loop should continue
        // (not break). We test this by trying to break out within 50 ms
        // using only the kick arm; the timer is infinite so only the kick
        // arm can fire.
        let broke_out = timeout(Duration::from_millis(50), async {
            loop {
                let elapsed = last_run_at.elapsed();
                let remaining = interval.saturating_sub(elapsed);
                tokio::select! {
                    _ = tokio::time::sleep(remaining) => { return true; }
                    _ = kick.notified() => {
                        let since_last = last_run_at.elapsed();
                        if since_last >= quiesce_window {
                            return true;
                        }
                        // absorbed — continue waiting
                    }
                }
            }
        })
        .await;

        // The timeout must fire (broke_out = Err) because the kick was
        // absorbed and the periodic timer (3600 s) never elapsed.
        assert!(
            broke_out.is_err(),
            "kick within quiesce window must be absorbed, not break out of wait",
        );
    }

    /// Phase 10 #31 acceptance (case 1 / merge_conflict alone): a
    /// chore that carries only the `merge_conflict` signal in the
    /// side table is routed to the conflict retire path by the
    /// polymorphic dispatch (and crucially NOT to the CI retire
    /// path). The `merge_conflict` row in `task_blocked_signals` is
    /// stamped `cleared_at` once the conflict resolves.
    #[tokio::test]
    async fn polymorphic_clear_routes_merge_conflict_signal() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/910";
        let (_product_id, chore) = make_chore_in_review(&db, "C-mc-only", pr);

        // Stage merge_conflict only — mark_chore_blocked_merge_conflict
        // upserts the side-table row as part of the same transaction
        // (Phase 10 #31).
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let staged: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        assert_eq!(staged, vec!["merge_conflict".to_owned()]);

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Mergeable=Clean, CI=Clean — but the side table only has
        // merge_conflict, so the polymorphic dispatch must NOT fire
        // on_ci_resolved (which would have been a no-op anyway, but
        // the new shape skips the unconditional call entirely).
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome =
            run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_cleared, 1);
        assert_eq!(outcome.ci_cleared, 0);

        // Side table row was stamped `cleared_at`.
        let active = db.active_blocked_signals(&chore).unwrap();
        assert!(active.is_empty(), "merge_conflict signal cleared; got {active:?}");
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Phase 10 #31/#32 acceptance (case 2 / ci_failure alone): a
    /// chore that carries only the `ci_failure` signal is routed to
    /// the CI retire path. Budget reset (#32) is observable: a chore
    /// with `ci_attempts_used = 2` lands at 0 after the cycle.
    #[tokio::test]
    async fn polymorphic_clear_routes_ci_failure_signal_and_resets_budget() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/911";
        let (_product_id, chore) = make_chore_in_review(&db, "C-ci-only", pr);

        // Stage ci_failure only (the production detect path would do
        // this via `on_ci_failure_detected` → `mark_chore_blocked_ci_failure`).
        db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE tasks SET ci_attempts_used = 2 WHERE id = ?1",
                rusqlite::params![chore],
            )
            .unwrap();
        }
        let staged: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        assert_eq!(staged, vec!["ci_failure".to_owned()]);

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome =
            run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.ci_cleared, 1, "polymorphic dispatch fired on_ci_resolved");
        assert_eq!(outcome.conflict_cleared, 0, "no merge_conflict signal => no conflict retire");

        let active = db.active_blocked_signals(&chore).unwrap();
        assert!(active.is_empty(), "ci_failure signal cleared; got {active:?}");
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert_eq!(
                    t.ci_attempts_used, 0,
                    "Phase 10 #32: full cycle resets budget to 0",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Phase 10 #31 acceptance (case 3 / both signals): when both
    /// `merge_conflict` and `ci_failure` rows are active in the side
    /// table, the polymorphic dispatch iterates both. Only the signal
    /// whose probe condition holds clears on a given pass; the other
    /// stays active. This mirrors the design's "each clears
    /// independently when its probe condition holds" acceptance.
    ///
    /// In production both signals being live simultaneously is rare
    /// (the engine's compose-order Q1 has conflict pre-empt CI), but
    /// the side-table can hold both rows for a window — e.g. when the
    /// `ci_failure` row pre-dates a freshly-detected conflict — so
    /// the dispatch's polymorphism must handle the case.
    #[tokio::test]
    async fn polymorphic_clear_each_signal_independent_when_both_active() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/912";
        let (_product_id, chore) = make_chore_in_review(&db, "C-both", pr);

        // Stage: the scalar `blocked_reason` lands on `ci_failure`
        // (its WHERE guard accepts `in_review`), and we hand-place a
        // sibling `merge_conflict` side-table row to simulate the
        // race window.
        db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO task_blocked_signals
                    (work_item_id, reason, attempt_id, created_at, cleared_at)
                 VALUES (?1, 'merge_conflict', NULL, '1700000000', NULL)",
                rusqlite::params![chore],
            )
            .unwrap();
        }
        let mut staged: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        staged.sort();
        assert_eq!(
            staged,
            vec!["ci_failure".to_owned(), "merge_conflict".to_owned()],
        );

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1: probe reports mergeable=Conflict, ci=Clean. The
        // dispatch must short-circuit before reaching either retire
        // path because `Conflict` mergeability routes to the
        // detect/idempotent path (not the Clean clear path). The
        // signals therefore stay active.
        probe.set(
            pr,
            PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Conflict,
                ci: OpenPrCiStatus::Clean,
            }),
        );
        let _ = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        let active_after_1: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        let mut active_after_1 = active_after_1;
        active_after_1.sort();
        assert_eq!(
            active_after_1,
            vec!["ci_failure".to_owned(), "merge_conflict".to_owned()],
            "Conflict mergeability must not clear either side-table row",
        );

        // Pass 2: probe reports mergeable=Clean, ci=Clean. The
        // dispatch's clean-branch iterates the side table and clears
        // the `merge_conflict` row (via on_resolved) and the
        // `ci_failure` row (via on_ci_resolved). Each fires
        // independently — neither hides the other.
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome =
            run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        // The conflict retire path is no-op against the side-table row
        // because the scalar is `ci_failure`; the WHERE guard in
        // `clear_chore_blocked_merge_conflict` misses. However, the
        // signal-row clear happens regardless: the dispatch's
        // polymorphic iteration sees both reasons and routes
        // each — the CI retire fires (scalar matches), and the
        // conflict retire is a cheap no-op as designed.
        assert_eq!(
            outcome.ci_cleared, 1,
            "ci_failure retired (scalar matched ci_failure)",
        );
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Phase 12 #41 — cross-flow ordering correctness. When a PR
    /// develops both a merge conflict and a CI failure
    /// simultaneously, the engine fires the conflict resolver first,
    /// the CI fixer only after the conflict resolves. The
    /// `task_blocked_signals` side table must reflect both signals
    /// being active and clearing in the right order:
    ///
    ///   * Pass 1 (mergeable=Conflict + ci=Failing): `merge_conflict`
    ///     becomes active. CI detection is *not* invoked (the
    ///     mergeability=Conflict arm in `sweep_one` short-circuits
    ///     before reaching the Clean branch where ci_watch fires).
    ///   * Pass 2 (the worker has pushed; mergeable=Clean +
    ///     ci=Failing): the `merge_conflict` signal clears (probe
    ///     condition holds) and the `ci_failure` detect path runs in
    ///     the same sweep, adding `ci_failure` to the side table.
    ///   * Pass 3 (mergeable=Clean + ci=Clean): `ci_failure` clears
    ///     and the parent ends back at `in_review`.
    #[tokio::test]
    async fn cross_flow_conflict_then_ci_fires_in_order() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/941";
        let (_product_id, chore) = make_chore_in_review(&db, "C-cross", pr);

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let failures = vec![RequiredCheckFailure {
            name: "ci/test".into(),
            conclusion: "FAILURE".into(),
            target_url: "https://buildkite.com/anthropic/mono/builds/1#job".into(),
            provider: CiProvider::Buildkite,
            provider_job_id: Some("job-1".into()),
        }];

        // Pass 1: Conflict + Failing.
        let mut p1 = PrLifecycleProbe {
            url: pr.into(),
            state: PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Conflict,
                ci: OpenPrCiStatus::Failing { failures: failures.clone() },
            }),
            base_ref_oid: Some("base-1".into()),
            head_ref_oid: Some("head-1".into()),
            head_ref_name: Some("feature".into()),
            base_ref_name: Some("main".into()),
            labels: Vec::new(),
            review: PrReviewState::Unknown,
            in_merge_queue: false,
        };
        probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
        let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(
            out1.conflict_flagged, 1,
            "conflict_watch must fire first on Conflict+Failing",
        );
        assert_eq!(
            out1.ci_flagged, 0,
            "ci_watch must NOT fire while mergeability=Conflict (design §Q1)",
        );
        let active1: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        assert_eq!(active1, vec!["merge_conflict".to_owned()]);

        // Worker resolves the conflict — head sha advances and the
        // mergeability flips to Clean. CI is still failing on the new
        // head sha. (The conflict resolution attempt row is not
        // exercised here — we go straight to the next probe.)
        p1.state = PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing { failures: failures.clone() },
        });
        p1.head_ref_oid = Some("head-2".into());
        probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
        let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(
            out2.conflict_cleared, 1,
            "merge_conflict retire fires in the Clean branch",
        );
        assert_eq!(
            out2.ci_flagged, 1,
            "ci_watch detect fires in the same Clean sweep once conflict cleared",
        );
        let active2: Vec<String> = db
            .active_blocked_signals(&chore)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        assert_eq!(
            active2,
            vec!["ci_failure".to_owned()],
            "after pass 2, only ci_failure is active",
        );

        // Pass 3: CI goes green. The ci_failure signal retires and
        // the parent returns to `in_review`.
        p1.state = PrLifecycleState::Open(OpenPrStatus::clean());
        probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
        let out3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(out3.ci_cleared, 1);
        assert!(db.active_blocked_signals(&chore).unwrap().is_empty());
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Acceptance test: a kick that arrives after the quiesce window
    /// has elapsed triggers an immediate pass (breaks out of the wait).
    #[tokio::test]
    async fn activation_kick_after_quiesce_window_triggers_pass() {
        use tokio::time::timeout;

        let kick = Arc::new(Notify::new());
        let quiesce_window = Duration::from_millis(1); // essentially instant
        let interval = Duration::from_secs(3600);

        // Simulate: last run finished a long time ago (100 ms > 1 ms quiesce).
        let last_run_at = Instant::now() - Duration::from_millis(100);

        // Fire a kick.
        kick.notify_one();

        // The 'wait loop should break out immediately because elapsed > quiesce.
        let broke_out = timeout(Duration::from_millis(500), async {
            loop {
                let elapsed = last_run_at.elapsed();
                let remaining = interval.saturating_sub(elapsed);
                tokio::select! {
                    _ = tokio::time::sleep(remaining) => { return true; }
                    _ = kick.notified() => {
                        let since_last = last_run_at.elapsed();
                        if since_last >= quiesce_window {
                            return true; // break out — trigger pass
                        }
                    }
                }
            }
        })
        .await;

        assert!(
            broke_out.is_ok(),
            "kick after quiesce window must break out of wait loop",
        );
    }

    /// Cold-path regression pin: when a conflict-resolution worker pushes
    /// a resolved branch but the engine's in-memory `StagedResolutionSignalCache`
    /// is empty (e.g. engine restarted between the push and the Stop hook),
    /// the merge-poller sweep must still detect the PR as mergeable and run
    /// the retire path — transitioning the parent back to `in_review` and
    /// marking the attempt `succeeded`.
    ///
    /// This is the signal-missed recovery scenario that the primary-path
    /// (on-Stop) shortcut cannot cover alone. The merge-poller sweep is the
    /// structural fallback.
    #[tokio::test]
    async fn merge_poller_recovers_conflict_resolution_when_signal_missed() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/700";
        let (product, chore) = make_chore_in_review(&db, "C-signal-missed", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let cube = Arc::new(RecordingCubeClient::default());

        // Pass 1: flip to blocked, then install the attempt (mirroring
        // Phase 3's worker-spawn path).
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
            None,
        )
        .await;
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 700,
                head_branch: "feature-700".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base-700".into()),
                head_sha_before: Some("head-700".into()),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "lease-700", "ws-700", "worker-700")
            .unwrap();

        // Simulate: the worker pushed and resolved the conflict but the
        // engine restarted — StagedResolutionSignalCache is empty and the
        // on-Stop primary path cannot fire. The PR is now MERGEABLE.
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
        let outcome = run_one_pass(
            &db,
            probe.as_ref(),
            publisher.as_ref(),
            Some(cube.as_ref() as &dyn CubeClient),
            None,
        )
        .await;
        assert_eq!(
            outcome.conflict_cleared, 1,
            "merge-poller must recover the conflict transition when the signal was missed",
        );

        // Parent in_review.
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert!(t.blocked_reason.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Attempt succeeded.
        let after = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
        assert_eq!(after.status, "succeeded");
    }

    // ── Bug B: late PR recovery ─────────────────────────────────────────────

    struct FixedPrDetector(Option<String>);

    #[async_trait]
    impl PrDetector for FixedPrDetector {
        async fn detect_pr(
            &self,
            _repo_remote_url: &str,
            _expected_branch: &str,
        ) -> Result<PrStatus> {
            Ok(match &self.0 {
                Some(url) => PrStatus::Fresh { url: url.clone() },
                None => PrStatus::None,
            })
        }
    }

    struct NoopPaneReleaser;

    #[async_trait]
    impl WorkerPaneReleaser for NoopPaneReleaser {
        async fn release_pane(&self, _run_id: &str) {}
    }

    struct NoopProbeQueuer;

    impl ProbeQueuer for NoopProbeQueuer {
        fn queue_probe(&self, _run_id: &str, _text: &str) {}
    }

    struct NoopCubeClient;

    #[async_trait]
    impl CubeClient for NoopCubeClient {
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unreachable!()
        }
        async fn lease_workspace(&self, _: &str, _: &str, _: Option<&str>) -> Result<CubeWorkspaceLease> {
            unreachable!()
        }
        async fn create_change(&self, _: &std::path::PathBuf, _: &str) -> Result<CubeChangeHandle> {
            unreachable!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unreachable!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(Vec::new())
        }
    }

    fn make_abandoned_chore_with_workspace(db: &WorkDb, name: &str) -> (String, String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: format!("Prod-{name}"),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
                force_duplicate: false,
            })
            .unwrap();
        let exec = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
                prefer_is_soft: false,
                pr_url: None,
            })
            .unwrap();
        let (exec, run) = db
            .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/ws/1")
            .unwrap();
        db.finish_execution_run(
            &exec.id, &run.id, "waiting_human", "completed", None, None, false, None,
        )
        .unwrap();
        // Simulate orphan sweep abandoning exec_A.
        db.mark_execution_redundant(&exec.id).unwrap();
        (product.id, chore.id, exec.id)
    }

    #[tokio::test]
    async fn run_one_pass_recovers_late_pr_for_abandoned_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_, chore_id, _exec_id) =
            make_abandoned_chore_with_workspace(&db, "late-pr-sweep-chore");

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let detector = Arc::new(FixedPrDetector(Some(
            "https://github.com/foo/bar/pull/77".into(),
        )));
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            Arc::new(NoopCubeClient),
            publisher.clone(),
            Arc::new(NoopPaneReleaser),
            Arc::new(NoopProbeQueuer),
        );

        let outcome = run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            None,
            Some(&handler),
        )
        .await;

        assert_eq!(
            outcome.late_pr_recovered, 1,
            "expected one late PR recovery, got: {outcome:?}",
        );

        let task = match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, "in_review");
        assert_eq!(
            task.pr_url.as_deref(),
            Some("https://github.com/foo/bar/pull/77")
        );
    }

    #[tokio::test]
    async fn run_one_pass_does_not_query_late_pr_candidates_without_handler() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_product_id, chore_id, _exec_id) =
            make_abandoned_chore_with_workspace(&db, "late-pr-no-handler");

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Passing completion_handler = None; late-PR sweep should be skipped.
        // Also seed the in_review list so total > 0 and the sweep actually runs.
        let pr_url = "https://github.com/foo/bar/pull/78";
        db.update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        probe.set(pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));

        let outcome = run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            None,
            None, // no handler
        )
        .await;

        assert_eq!(
            outcome.late_pr_recovered, 0,
            "late_pr_recovered must be 0 when no handler is wired",
        );
    }

    /// Build a chore that is `blocked: ci_failure` with a PR and a live
    /// worker execution still attached (status `running`). Mirrors the
    /// issue-#898 scenario: a worker that fixed CI but is left polling.
    /// Returns `(product_id, chore_id, execution_id)`.
    fn make_blocked_ci_chore_with_live_worker(
        db: &WorkDb,
        name: &str,
        pr: &str,
    ) -> (String, String, String) {
        let (product_id, chore) = make_chore_in_review(db, name, pr);
        db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
        let exec = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
                prefer_is_soft: false,
                pr_url: None,
            })
            .unwrap();
        let (exec, _run) = db
            .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/ws/1")
            .unwrap();
        // Precondition: the worker is live for the task.
        assert!(
            db.get_live_execution_for_work_item(&chore, "")
                .unwrap()
                .is_some(),
            "setup: worker should be live before the sweep",
        );
        (product_id, chore, exec.id)
    }

    /// Issue #898: when the engine auto-transitions a `blocked: ci_failure`
    /// task back to `in_review` (CI detected green), the live worker that
    /// was running it must be force-stopped — it has nothing useful left
    /// to do and otherwise holds its slot indefinitely. The task itself
    /// stays in Review (force-stop's demotion guard only fires on
    /// `active`).
    #[tokio::test]
    async fn ci_resolved_stops_live_worker_and_keeps_task_in_review() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let pr = "https://github.com/foo/bar/pull/898";
        let (_product_id, chore, exec_id) =
            make_blocked_ci_chore_with_live_worker(&db, "C-898-stop", pr);

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            Arc::new(FixedPrDetector(None)),
            Arc::new(NoopCubeClient),
            publisher.clone(),
            Arc::new(NoopPaneReleaser),
            Arc::new(NoopProbeQueuer),
        );

        let outcome = run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            None,
            Some(&handler),
        )
        .await;

        assert_eq!(outcome.ci_cleared, 1, "ci_failure retired to in_review");
        assert_eq!(
            outcome.worker_stopped_on_review, 1,
            "the live worker for the task was force-stopped, got: {outcome:?}",
        );

        // Task stays in Review — NOT demoted back to todo.
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        // The worker execution is now terminal and no longer live.
        assert_eq!(db.get_execution(&exec_id).unwrap().status, "cancelled");
        assert!(
            db.get_live_execution_for_work_item(&chore, "")
                .unwrap()
                .is_none(),
            "no live worker should remain for the task",
        );
    }

    /// Without a completion handler wired (tests / cold-path), the CI
    /// retire path still fires but the worker-stop is a no-op — the
    /// counter stays 0 and the execution is left untouched.
    #[tokio::test]
    async fn ci_resolved_without_handler_does_not_stop_worker() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let pr = "https://github.com/foo/bar/pull/899";
        let (_product_id, chore, exec_id) =
            make_blocked_ci_chore_with_live_worker(&db, "C-898-nohandler", pr);

        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));

        let outcome = run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            None,
            None, // no handler
        )
        .await;

        assert_eq!(outcome.ci_cleared, 1, "ci_failure still retires");
        assert_eq!(
            outcome.worker_stopped_on_review, 0,
            "no worker-stop without a handler, got: {outcome:?}",
        );
        // Execution untouched — still live.
        assert_eq!(db.get_execution(&exec_id).unwrap().status, "running");
    }

    // ----- parse_dequeue_events_response (merge-queue reason case T770/T771) -----

    /// GitHub's GraphQL API returns `reason` in lowercase snake_case
    /// ("failed_checks") even though the schema documents the enum as
    /// FAILED_CHECKS.  The parser must accept the lowercase form.
    #[test]
    fn parse_dequeue_events_response_accepts_lowercase_failed_checks() {
        let body = br#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "nodes": [
                                {
                                    "reason": "failed_checks",
                                    "beforeCommit": {"oid": "abc123def456"}
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let events = parse_dequeue_events_response(body);
        assert_eq!(events.len(), 1, "lowercase 'failed_checks' must be surfaced");
        assert_eq!(events[0].reason, "failed_checks");
        assert_eq!(events[0].before_commit_oid.as_deref(), Some("abc123def456"));
    }

    /// The schema-documented uppercase form must also be accepted for
    /// forward-compatibility (in case GitHub normalises casing in future).
    #[test]
    fn parse_dequeue_events_response_accepts_uppercase_failed_checks() {
        let body = br#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "nodes": [
                                {
                                    "reason": "FAILED_CHECKS",
                                    "beforeCommit": {"oid": "def456abc789"}
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let events = parse_dequeue_events_response(body);
        assert_eq!(events.len(), 1, "uppercase 'FAILED_CHECKS' must also be surfaced");
        assert_eq!(events[0].before_commit_oid.as_deref(), Some("def456abc789"));
    }

    /// Non-FAILED_CHECKS reasons (manual dequeue, merge conflict, etc.) must
    /// be silently discarded — they must not trigger the ci_failure path.
    #[test]
    fn parse_dequeue_events_response_filters_non_failed_checks() {
        let body = br#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "nodes": [
                                {"reason": "dequeued",       "beforeCommit": {"oid": "sha1"}},
                                {"reason": "merge_conflict", "beforeCommit": {"oid": "sha2"}},
                                {"reason": "queue_cleared",  "beforeCommit": {"oid": "sha3"}},
                                {"reason": "failed_checks",  "beforeCommit": {"oid": "sha4"}}
                            ]
                        }
                    }
                }
            }
        }"#;
        let events = parse_dequeue_events_response(body);
        assert_eq!(events.len(), 1, "only failed_checks must be surfaced");
        assert_eq!(events[0].before_commit_oid.as_deref(), Some("sha4"));
    }

    /// `beforeCommit` can be null when GitHub omits it. The event must
    /// still be returned (with `before_commit_oid = None`) so the caller
    /// can decide how to handle it.
    #[test]
    fn parse_dequeue_events_response_handles_null_before_commit() {
        let body = br#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "nodes": [
                                {"reason": "failed_checks", "beforeCommit": null}
                            ]
                        }
                    }
                }
            }
        }"#;
        let events = parse_dequeue_events_response(body);
        assert_eq!(events.len(), 1, "null beforeCommit must not drop the event");
        assert!(events[0].before_commit_oid.is_none());
    }

    /// An empty nodes array returns an empty vec without panicking.
    #[test]
    fn parse_dequeue_events_response_empty_nodes() {
        let body = br#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {"nodes": []}
                    }
                }
            }
        }"#;
        assert!(parse_dequeue_events_response(body).is_empty());
    }

    /// Acceptance test for T831 / the CI-status invalidation gap: once a
    /// failure is recorded (`ci_required_state = "fail"`, `blocked: ci_failure`),
    /// a subsequent clean probe must propagate the recovery transition — the
    /// `blocked_ci` re-poll set must re-check the PR and update the task's
    /// `ci_required_state` to `"success"` so the kanban CI indicator clears.
    #[tokio::test]
    async fn ci_required_state_clears_when_rollup_recovers_to_success() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/702";
        let (product, chore) = make_chore_in_review(&db, "C-ci-state-clear", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1: statusCheckRollup reports a FAILURE — simulates the initial
        // detection sweep that blocks the task.
        probe.states.lock().unwrap().insert(
            pr.to_owned(),
            Ok(probe_ci_failing(pr, "head-1")),
        );
        let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(out1.ci_flagged, 1, "first sweep must detect and block on CI failure");

        // ci_required_state should reflect the failing rollup after detection.
        let ci_state_after_fail = match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => t.ci_required_state,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            ci_state_after_fail.as_deref(),
            Some("fail"),
            "ci_required_state must be 'fail' once the failing rollup is recorded",
        );

        // Pass 2: statusCheckRollup flips to SUCCESS — simulates CI recovering
        // (developer fixed the issue or flaky test re-ran green). The
        // blocked_ci re-poll set must re-check this PR and propagate the
        // recovery, clearing both the block and the CI indicator.
        probe.states.lock().unwrap().insert(
            pr.to_owned(),
            Ok(probe_ci_clean(pr, "head-1")),
        );
        let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(out2.ci_cleared, 1, "clean probe must retire the ci_failure block");

        let t = match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(t.status, "in_review", "task must be back in_review after CI recovery");
        assert!(t.blocked_reason.is_none(), "blocked_reason must be cleared");
        assert_eq!(
            t.ci_required_state.as_deref(),
            Some("success"),
            "ci_required_state must be 'success' after the rollup recovers — \
             this drives the PrCiIndicator green checkmark on the kanban card",
        );

        // A pr_poll_state_updated event must have been emitted so the macOS
        // kanban refreshes the CI indicator without waiting for a user action.
        let all_events = publisher.work_events.lock().await.clone();
        let has_poll_update = all_events
            .iter()
            .any(|(p, w, r)| p == &product && w == &chore && r == "pr_poll_state_updated");
        assert!(
            has_poll_update,
            "pr_poll_state_updated must be emitted when ci_required_state changes; \
             got: {all_events:?}",
        );
    }

    /// When a task is `blocked: ci_failure` at the time its PR is merged, any
    /// pending `ci_remediations` rows must be abandoned so the macOS kanban
    /// clears the "ci failing" badge. Without this cleanup the pending row
    /// causes the badge to reappear on every `sendListCiRemediations` call
    /// (T831 repro path).
    #[tokio::test]
    async fn merge_of_ci_blocked_pr_clears_badge() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/703";
        let (_product, chore) = make_chore_in_review(&db, "C-merge-clears-badge", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1: CI fails — chore flips to blocked: ci_failure with a pending
        // ci_remediations row.
        probe.states.lock().unwrap().insert(
            pr.to_owned(),
            Ok(probe_ci_failing(pr, "head-1")),
        );
        let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(out1.ci_flagged, 1);
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "blocked"),
            other => panic!("expected chore, got {other:?}"),
        }

        // Verify the pending ci_remediations row exists (its presence is what
        // drives the badge via sendListCiRemediations).
        let active = db.active_ci_remediation_for_work_item(&chore).unwrap();
        assert!(active.is_some(), "a pending ci_remediations row must exist after detection");

        // Pass 2: GitHub reports the PR as MERGED while CI is still failing on
        // the head branch (force-merge / merge-queue scenario). The sweep must
        // mark the pending row abandoned so it no longer shows up as
        // pending/running in the remediations list.
        probe.states.lock().unwrap().insert(
            pr.to_owned(),
            Ok(PrLifecycleProbe {
                url: pr.to_owned(),
                state: PrLifecycleState::Merged,
                base_ref_oid: None,
                head_ref_oid: None,
                head_ref_name: None,
                base_ref_name: None,
                labels: vec![],
                review: PrReviewState::Unknown,
                in_merge_queue: false,
            }),
        );
        let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(out2.merged, 1, "merge must be detected");

        // Task must be done.
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore, got {other:?}"),
        }

        // The pending ci_remediations row must now be abandoned — a pending
        // row here would cause sendListCiRemediations to re-set the "ci
        // failing" badge on every app restart even though the task is done.
        let still_active = db.active_ci_remediation_for_work_item(&chore).unwrap();
        assert!(
            still_active.is_none(),
            "pending ci_remediations row must be abandoned on PR merge; \
             badge would persist on app restart otherwise",
        );
    }
}
