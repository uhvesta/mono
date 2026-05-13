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
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use crate::ci_watch;
use crate::completion::{StopOutcome, WorkerCompletionHandler};
use crate::conflict_watch;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::work::{PendingMergeCheck, WorkDb};

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
/// flags it `manual_action_required` (design §Q4).
fn is_failure_conclusion(c: &str) -> bool {
    matches!(
        c.to_ascii_uppercase().as_str(),
        "FAILURE"
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
                "state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,headRefOid,headRefName,baseRefName,labels,statusCheckRollup",
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
                });
            }
            return Err(anyhow!(
                "`gh pr view {pr_url}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_probe_json(pr_url, &stdout)
    }
}

/// Parse the raw JSON document `gh pr view --json …` returns into a
/// [`PrLifecycleProbe`]. Pure function so the parsing rules can be
/// unit-tested without shelling out. A document that fails to parse
/// is *not* treated as conflicting / failing — we fall back to an
/// `Open(clean)` shape so a malformed gh response can't fire a
/// false-positive blocked flip. Real failures (auth, network) come
/// through as `Err` from the shelling-out layer, not via this path.
fn parse_probe_json(url: &str, body: &str) -> Result<PrLifecycleProbe> {
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
    let ci = classify_ci(&rollup);
    let state = classify_state(raw_state, merged_at, mergeable, merge_state_status, ci);
    Ok(PrLifecycleProbe {
        url: url.to_owned(),
        state,
        base_ref_oid,
        head_ref_oid,
        head_ref_name,
        base_ref_name,
        labels,
    })
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
///   3. For each surviving leaf:
///        - If `status != COMPLETED` and the conclusion isn't already a
///          recognised pass/fail signal, it's `InFlight`.
///        - If `conclusion ∈ failure-set` → contributes a failure entry.
///        - Otherwise (success / neutral / skipped / missing) → no-op.
///   4. If any failures collected → `Failing`. Else if any leaf was
///      InFlight → `InFlight`. Else `Clean`.
fn classify_ci(leaves: &[serde_json::Value]) -> OpenPrCiStatus {
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
        // A leaf is in-flight when its status is one of GitHub's
        // pending-shape values OR when status==COMPLETED but the
        // conclusion is still empty (briefly, post-completion).
        let status_in_flight = matches!(
            status.as_str(),
            "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" | ""
        );
        let conclusion_empty = conclusion.is_empty();
        if conclusion_empty || status_in_flight {
            // No terminal conclusion yet — count toward InFlight but
            // don't fail.
            if conclusion_empty {
                any_in_flight = true;
                continue;
            }
        }
        if is_failure_conclusion(&conclusion) {
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
            continue;
        }
        if is_pass_conclusion(&conclusion) {
            continue;
        }
        // Unknown conclusion shape — treat as in-flight rather than
        // misclassifying it as a failure.
        any_in_flight = true;
    }

    if !failures.is_empty() {
        return OpenPrCiStatus::Failing { failures };
    }
    if any_in_flight {
        return OpenPrCiStatus::InFlight;
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
    /// Number of stranded `conflict_resolutions` attempts (status
    /// `pending`, no live execution) for which a fresh execution was
    /// re-emitted. Covers the engine-restart / worker-die gap where
    /// no normal sweep would otherwise rescue the attempt.
    pub conflict_redispatched: usize,
}

impl SweepOutcome {
    fn total_transitions(self) -> usize {
        self.merged
            + self.conflict_flagged
            + self.conflict_cleared
            + self.ci_flagged
            + self.ci_cleared
            + self.pr_recheck_recovered
            + self.conflict_redispatched
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
    let stranded_attempts = match work_db.list_stranded_conflict_resolution_attempts() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list stranded conflict resolution attempts",
            );
            Vec::new()
        }
    };
    let total = in_review.len()
        + blocked_conflict.len()
        + blocked_ci.len()
        + pending_pr_recheck.len()
        + stranded_attempts.len();
    if total == 0 {
        return SweepOutcome::default();
    }
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
        sweep_one(work_db, probe, publisher, cube_client, candidate, &mut outcome).await;
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
    for attempt in &stranded_attempts {
        if conflict_watch::rescue_stranded_attempt(work_db, publisher, attempt).await {
            outcome.conflict_redispatched += 1;
        }
    }
    outcome
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
        // or the execution moved on between list and recheck.
        StopOutcome::AwaitingInput
        | StopOutcome::DetectorFailed
        | StopOutcome::StalePr { .. }
        | StopOutcome::EmptyDiffPr { .. }
        | StopOutcome::AlreadyTerminal
        | StopOutcome::UnknownExecution
        | StopOutcome::NoWorkspace
        | StopOutcome::DbError => {}
    }
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
    match &probe_result.state {
        PrLifecycleState::Merged => {
            if mark_merged(work_db, publisher, candidate).await {
                outcome.merged += 1;
            }
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
                    if conflict_watch::on_conflict_detected(
                        work_db,
                        publisher,
                        candidate,
                        &probe_result,
                    )
                    .await
                    {
                        outcome.conflict_flagged += 1;
                    }
                }
                OpenPrMergeability::Clean => {
                    // Conflict-side retire: idempotent against `in_review`
                    // rows (the WHERE guard misses); the actual transition
                    // only fires when the row was previously blocked.
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
                    // CI-side dispatch. `Failing` fans out to the
                    // CI-watch detect path; `Clean`/`InFlight` drive the
                    // CI-watch retire path (which is also a cheap no-op
                    // for rows that aren't blocked on a CI signal).
                    match ci {
                        OpenPrCiStatus::Failing { failures } => {
                            if ci_watch::on_ci_failure_detected(
                                work_db,
                                publisher,
                                candidate,
                                &probe_result,
                                failures,
                            )
                            .await
                            {
                                outcome.ci_flagged += 1;
                            }
                        }
                        OpenPrCiStatus::Clean => {
                            if ci_watch::on_ci_resolved(
                                work_db,
                                publisher,
                                candidate,
                                &probe_result.labels,
                            )
                            .await
                            {
                                outcome.ci_cleared += 1;
                            }
                        }
                        OpenPrCiStatus::InFlight => {
                            // Wait — the auto-retire path requires all
                            // checks at SUCCESS (design §Q5). InFlight
                            // is the explicit "don't act yet" leaf.
                        }
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
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    probe: Arc<dyn MergeProbe>,
    publisher: Arc<dyn ExecutionPublisher>,
    cube_client: Arc<dyn CubeClient>,
    completion_handler: Arc<WorkerCompletionHandler>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                probe.as_ref(),
                publisher.as_ref(),
                Some(cube_client.as_ref()),
                Some(completion_handler.as_ref()),
            )
            .await;
            if outcome.total_transitions() > 0 {
                tracing::info!(
                    merged = outcome.merged,
                    conflict_flagged = outcome.conflict_flagged,
                    conflict_cleared = outcome.conflict_cleared,
                    conflict_redispatched = outcome.conflict_redispatched,
                    ci_flagged = outcome.ci_flagged,
                    ci_cleared = outcome.ci_cleared,
                    pr_recheck_recovered = outcome.pr_recheck_recovered,
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
        ConflictResolutionInsertInput, CreateChoreInput, CreateProductInput, CreateProjectInput,
        CreateTaskInput, WorkDb, WorkItem, WorkItemPatch,
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
        assert!(publisher.work_events.lock().await.is_empty());
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

        // Event trail: blocked → resolved.
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
            let probe = parse_probe_json("https://example.test/pr/1", &body).unwrap();
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
        let probe = parse_probe_json("https://example.test/pr/2", &body).unwrap();
        assert_eq!(
            probe.labels,
            vec!["needs-review".to_owned(), "boss/no-auto-rebase".to_owned()],
        );

        let body_empty = json_doc(
            "OPEN", "", "MERGEABLE", "CLEAN", "abc", "", &[], serde_json::json!([]),
        );
        let probe_empty = parse_probe_json("https://example.test/pr/3", &body_empty).unwrap();
        assert!(probe_empty.labels.is_empty());
    }

    /// `(state × mergeability × ci-leaf-set)` matrix for the CI
    /// predicate. Exercises the latest-leaf-per-name collapse, the
    /// required/not-required filter, and the closed conclusion set
    /// from design §Q1 / Phase 8 #21.
    #[test]
    fn parse_probe_covers_ci_leaf_set_matrix() {
        struct Case {
            label: &'static str,
            rollup: serde_json::Value,
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
                label: "no rollup → Clean (legacy PRs with branch protection off)",
                rollup: serde_json::json!([]),
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "all required checks SUCCESS → Clean",
                rollup: serde_json::json!([success_check("ci/build"), success_check("ci/test")]),
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "one required check FAILURE → Failing",
                rollup: serde_json::json!([
                    success_check("ci/build"),
                    failing_check("ci/test", "FAILURE", ""),
                ]),
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
                expect_ci: OpenPrCiStatus::Clean,
            },
            Case {
                label: "later leaf wins for the same name (re-run FAILURE masks earlier success)",
                rollup: serde_json::json!([
                    success_check("ci/test"),
                    failing_check("ci/test", "FAILURE", ""),
                ]),
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
                expect_ci: OpenPrCiStatus::InFlight,
            },
            Case {
                label: "STARTUP_FAILURE counts as failure (engine pre-triages to retrigger)",
                rollup: serde_json::json!([failing_check("ci/build", "STARTUP_FAILURE", "")]),
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
        ];
        for case in cases {
            let body = json_doc(
                "OPEN", "", "MERGEABLE", "CLEAN", "abc", "head-1", &[], case.rollup.clone(),
            );
            let probe = parse_probe_json("https://example.test/pr/ci", &body).unwrap();
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
        let probe = parse_probe_json("https://example.test/pr/both", &body).unwrap();
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
        assert!(publisher.work_events.lock().await.is_empty());
    }

    /// Helper: seed a chore into `blocked: merge_conflict` with a pending
    /// `conflict_resolutions` row and return `(product_id, chore_id, attempt_id)`.
    fn make_chore_blocked_with_pending_attempt(
        db: &WorkDb,
        name: &str,
        pr_url: &str,
    ) -> (String, String, String) {
        let (product_id, chore_id) = make_chore_in_review(db, name, pr_url);
        db.mark_chore_blocked_merge_conflict(&chore_id, pr_url)
            .unwrap();
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product_id.clone(),
                work_item_id: chore_id.clone(),
                pr_url: pr_url.into(),
                pr_number: 0,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("sha-base".into()),
                head_sha_before: Some("sha-head".into()),
            })
            .unwrap()
            .expect("attempt should be inserted (not UNIQUE collision)");
        (product_id, chore_id, attempt.id)
    }

    /// Flip `products.auto_pr_maintenance_enabled` directly on the
    /// SQLite file so opt-out tests can drive the gate without
    /// exposing a setter that production code doesn't need.
    fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
            rusqlite::params![product_id, if enabled { 1 } else { 0 }],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn stranded_pending_attempt_with_no_execution_gets_redispatched() {
        // (a) A stranded `pending` attempt with no live execution gets a
        // new execution emitted and `conflict_redispatched` increments.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/900";
        let (_product, chore, _attempt) =
            make_chore_blocked_with_pending_attempt(&db, "C-stranded", pr);

        // The probe still reports Conflict (the row is blocked, so the
        // probe sweep's `on_conflict_detected` will WHERE-guard miss —
        // the stranded sweep is the only path that creates an execution).
        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_redispatched, 1);

        // A ready execution should now exist for the chore.
        let ready = db.list_ready_executions().unwrap();
        assert!(
            ready
                .iter()
                .any(|e| e.work_item_id == chore && e.kind == "conflict_resolution"),
            "expected a ready conflict_resolution execution; got {ready:?}",
        );
    }

    #[tokio::test]
    async fn stranded_attempt_with_live_ready_execution_is_skipped() {
        // (b) An attempt with a live `ready` execution is NOT re-dispatched.
        // The query checks status IN ('ready','running','waiting_human');
        // 'ready' is the representative live case here.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/901";
        let (_, chore, _) = make_chore_blocked_with_pending_attempt(&db, "C-live-ready", pr);

        // Create a `ready` execution so the stranded-sweep exclusion kicks in.
        db.create_execution(crate::work::CreateExecutionInput {
            work_item_id: chore.clone(),
            kind: "conflict_resolution".to_owned(),
            status: Some("ready".to_owned()),
            repo_remote_url: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_redispatched, 0);

        // Only the one pre-existing execution should exist.
        let ready = db.list_ready_executions().unwrap();
        assert_eq!(
            ready
                .iter()
                .filter(|e| e.work_item_id == chore && e.kind == "conflict_resolution")
                .count(),
            1,
            "no duplicate execution should have been created",
        );
    }

    #[tokio::test]
    async fn abandoned_stranded_attempt_is_not_rescued() {
        // (c) An `abandoned` (`failed` / `succeeded`) attempt must NOT be
        // rescued — the churn guard or human owns that path. We use the
        // `failed` terminal state to represent both abandoned and failed rows.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/903";
        let (product, chore) = make_chore_in_review(&db, "C-abandoned", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 0,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("sha-903".into()),
                head_sha_before: None,
            })
            .unwrap()
            .expect("attempt should insert");
        // Terminal status — churn guard or human abandoned this.
        db.mark_conflict_resolution_failed(&attempt.id, "worker_died_terminally")
            .unwrap();

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_redispatched, 0);
        assert!(db.list_ready_executions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn opted_out_product_stranded_attempt_is_skipped() {
        // (d) A stranded attempt for an opted-out product is skipped.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/904";
        let (product, chore, _attempt) =
            make_chore_blocked_with_pending_attempt(&db, "C-optout-stranded", pr);

        set_product_auto_pr_maintenance(&db_path, &product, false);

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome.conflict_redispatched, 0);
        assert!(db.list_ready_executions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stranded_attempt_redispatch_then_worker_die_redispatches_again() {
        // Integration: simulate worker-die-before-PR-update — the
        // stranded sweep re-dispatches. If the execution is then
        // cancelled (worker die) without marking the attempt terminal,
        // the next sweep re-dispatches again.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/905";
        let (_product, chore, _attempt) =
            make_chore_blocked_with_pending_attempt(&db, "C-redispatch-cycle", pr);

        let probe = StubProbe::new();
        probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
        let publisher = Arc::new(RecordingPublisher::default());

        // Pass 1: stranded attempt gets an execution.
        let outcome1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome1.conflict_redispatched, 1);
        let ready1 = db.list_ready_executions().unwrap();
        assert_eq!(
            ready1
                .iter()
                .filter(|e| e.work_item_id == chore && e.kind == "conflict_resolution")
                .count(),
            1,
        );

        // Simulate worker die: cancel the execution (terminal) without
        // touching the attempt row (attempt stays `pending`).
        let exec_id = ready1
            .iter()
            .find(|e| e.work_item_id == chore && e.kind == "conflict_resolution")
            .unwrap()
            .id
            .clone();
        db.cancel_execution(&exec_id).unwrap();

        // Pass 2: no live execution → stranded sweep fires again.
        let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(outcome2.conflict_redispatched, 1);
        assert_eq!(
            db.list_ready_executions()
                .unwrap()
                .iter()
                .filter(|e| e.work_item_id == chore && e.kind == "conflict_resolution")
                .count(),
            1,
        );
    }
}
