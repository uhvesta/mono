//! Worker completion detection.
//!
//! `PaneSpawnRunner` returns `WaitingHuman` immediately after spawning
//! the worker pane, so the run row is recorded as `completed` before
//! the worker has actually done any work. The execution sits in
//! `waiting_human` with the cube lease retained, and the linked
//! task/chore stays in `active` (kanban "Doing"). Without something
//! else driving the lifecycle, completed work just sits in Doing
//! forever — that is the bug this module exists to close.
//!
//! ## Detection
//!
//! The primary signal is the in-memory PR-URL staging cache populated
//! by the `PostToolUse` hook for `gh pr create` / `gh pr view` /
//! `gh pr edit`: when the worker's hook stream carries a PR URL we
//! finalize the work item against it without touching git or
//! GitHub at all.
//!
//! The cold-path fallback (incident 001, AI #6) handles the case where
//! staging is empty (engine restart, hook miss, etc.) by querying
//! `gh pr list --head <branch>` for the PR whose head matches the
//! engine-supplied per-execution branch name. The branch name is
//! derived deterministically from `execution_id` (see
//! [`expected_branch_name`]) and is injected into the worker prompt,
//! so workers push to the name the engine gave them — sibling workers
//! in other cube workspaces have different execution IDs and therefore
//! cannot collide.
//!
//! The branch-keyed query replaces the previous SHA-keyed
//! `jj_candidate_commit_shas` + `gh api commits/{sha}/pulls` recipe,
//! which was structurally unsafe under cube's shared
//! `.jj/repo/store/git`: bookmarks pushed by ANY concurrent worker
//! were visible from EVERY workspace's `jj log`, so the detector
//! routinely matched a sibling's bookmark and bound the wrong PR.
//! See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` for
//! the full incident write-up.
//!
//! Merges that happen *after* the worker exited are detected by a
//! periodic poller wired in `app.rs`, which calls
//! [`WorkDb::mark_chore_pr_merged`] for any chore in `in_review`
//! whose `pr_url` is now in a merged GitHub state.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use boss_protocol::{
    AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_PRODUCED_TASK,
    AUTOMATION_OUTCOME_SKIPPED, Attention, AttentionGroup, BranchNaming, CREATED_VIA_CI_FIX_PREFIX,
    CREATED_VIA_MERGE_CONFLICT_PREFIX, CREATED_VIA_PR_REVIEW_PREFIX, CreateRevisionInput,
    ExecutionKind, ExecutionStatus, FrontendEvent, TaskKind,
};

use crate::attentions_detector;
use crate::automation_triage::{TriageDecision, parse_triage_decision};
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::design_detector;
use crate::merge_poller::{
    MergeProbe, NoopMergeProbe, OpenPrCiStatus, OpenPrMergeability, PrLifecycleState,
    update_pr_poll_state,
};
use crate::metrics::Registry;
use crate::nudge_breaker::{DEFAULT_MAX_UNPRODUCTIVE_NUDGES, NudgeBreaker, NudgeDecision};
use crate::work::{
    CreateAttentionItemInput, CreateExecutionInput, PendingMergeCheck, TaskStatus, WorkDb,
    WorkItem, WorkerPrCompletionTarget,
};

// Phase-3 counter handles for the PR URL capture paths. The primary path
// fires when the PostToolUse staging cache already holds the URL; the
// reconstruction path fires when the cold-path `detect_pr` fallback is
// invoked instead.
crate::register_counter!(
    PR_URL_CAPTURE_PRIMARY_HIT,
    "pr_url_capture.primary_path.hit",
    "on_stop / recheck_for_pr found a staged PR URL and skipped the detector.",
);
crate::register_counter!(
    PR_URL_CAPTURE_RECONSTRUCTION_HIT,
    "pr_url_capture.reconstruction_path.hit",
    "detect_pr cold-path fallback was invoked (staging cache empty).",
);
crate::register_counter!(
    PR_URL_CAPTURE_RECONSTRUCTION_FAILED,
    "pr_url_capture.reconstruction_path.failed",
    "detect_pr cold-path fallback returned Err (network / date-format class).",
);
crate::register_counter!(
    PR_RECHECK_STAGED_BRANCH_MISMATCH,
    "pr_url_capture.recheck_staged.branch_mismatch",
    "staged URL's PR branch did not match execution's expected branch; URL was dropped.",
);

/// Register all PR-URL-capture counter handles with `registry`. Called from
/// [`crate::metrics::init_all`] at engine startup so duplicate-name panics
/// surface at boot rather than at the first counter increment.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&PR_URL_CAPTURE_PRIMARY_HIT);
    registry.register_counter(&PR_URL_CAPTURE_RECONSTRUCTION_HIT);
    registry.register_counter(&PR_URL_CAPTURE_RECONSTRUCTION_FAILED);
    registry.register_counter(&PR_RECHECK_STAGED_BRANCH_MISMATCH);
}

/// Catch-all `failure_reason` stamped on a `conflict_resolutions` row
/// when the bound worker exits without pushing and without otherwise
/// classifying the failure via `boss engine conflicts mark-failed`
/// (design Q5 / Phase 4 #11). The activity-feed surface renders it
/// loudly so the user knows the engine gave up rather than churning.
pub const CONFLICT_NO_PUSH_REASON: &str = "no_push_no_stop_condition";

/// Catch-all `failure_reason` stamped on a `ci_remediations` row when
/// the bound worker exits without pushing and without otherwise
/// classifying the outcome via `boss engine ci mark-failed`
/// (design §Phase 10 #33). Mirrors [`CONFLICT_NO_PUSH_REASON`]; the
/// name diverges to make audits unambiguous about which flow the
/// catch-all fired in.
pub const CI_NO_PUSH_REASON: &str = "no_push_no_classification";

/// Result of a [`WorkerPaneReleaser::release_pane`] call. Tells the
/// caller whether a live worker slot was actually found and reaped — the
/// signal that gates the cube-lease release in [`WorkerCompletionHandler::force_release`].
///
/// The distinction exists to close the T981 mid-spawn-cancel collision:
/// a worker whose pid has not yet materialized has no mapped slot, so the
/// pane release is a no-op (`NoLiveWorker`). Freeing its cube lease at
/// that point would hand a still-to-be-occupied workspace back to cube,
/// which then re-leases it to another execution — two live processes,
/// one working tree. The lease must stay held until the occupant is
/// genuinely gone; the in-flight run reaps + releases once its spawn
/// settles (see `PaneSpawnRunner::run_execution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneReleaseOutcome {
    /// A mapped worker slot was found: its pane was torn down and its OS
    /// process tree signalled (SIGTERM, escalating to SIGKILL). The
    /// workspace is no longer occupied, so the caller may free the lease.
    Reaped,
    /// No slot was mapped for the run. Either the worker already released
    /// (idempotent second call) or — the case this distinction exists for
    /// — it is still mid-spawn with no pid yet, so nothing could be
    /// reaped. The caller MUST NOT free the cube lease on this outcome.
    NoLiveWorker,
}

/// Asks the registered app session to tear down the libghostty pane
/// hosting `run_id`. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
/// The completion handler calls this after a successful cube lease
/// release on PR detection so the Workers grid pane disappears.
///
/// Returns [`PaneReleaseOutcome`] so the caller can decide whether it is
/// safe to release the cube lease (only when a live worker was reaped).
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome;
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs). Reports `Reaped` so the
/// lease-release path is unchanged for setups without a pane subsystem.
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

/// What GitHub reports about a PR associated with the worker's
/// local commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatus {
    /// No PR is associated with any of the worker's local commits.
    None,
    /// PR exists and at least one of the worker's local commit shas
    /// matches the PR's head — nothing local is unpushed.
    Fresh { url: String },
    /// PR exists, but the worker's local commits are ahead of the
    /// PR's pushed head sha. Treat as "no PR yet" for completion
    /// purposes; the worker is probed to push.
    Stale { url: String, reason: String },
    /// PR exists and head_match, but `changed_files == 0` — the worker
    /// pushed a commit with no file changes. Do not advance to
    /// `in_review`; probe the worker to make real edits or close the PR.
    EmptyDiff { url: String },
    /// PR exists and is already merged. Move the work item straight
    /// to `done`.
    Merged { url: String },
    /// PR exists but was closed without merging. The work item
    /// should not advance — surface like "no PR" so the worker can
    /// decide whether to reopen / open a new one.
    Closed { url: String },
}

impl PrStatus {
    /// PR url, regardless of state.
    pub fn url(&self) -> Option<&str> {
        match self {
            PrStatus::None => None,
            PrStatus::Fresh { url }
            | PrStatus::Stale { url, .. }
            | PrStatus::EmptyDiff { url }
            | PrStatus::Merged { url }
            | PrStatus::Closed { url } => Some(url),
        }
    }
}

/// Default worker branch-name prefix when the product's branch-naming
/// strategy is [`BranchNaming::BossExecPrefix`]. Preserves the
/// historical `boss/exec_<id>` shape so existing setups are unchanged.
pub const DEFAULT_WORKER_BRANCH_PREFIX: &str = "boss/";

/// Engine-supplied branch name a worker must push to when opening
/// the PR for an execution. The exact shape depends on the execution's
/// [`BranchNaming`] strategy (snapshotted from the product's
/// `editorial_rules.branch_naming` at spawn time) and on the execution's
/// frozen `worker_branch_prefix` (snapshotted from the product's
/// `Product::worker_branch_prefix` column):
///
/// - [`BranchNaming::BossExecPrefix`] (default): `<prefix><execution_id>`,
///   where `<prefix>` is `worker_branch_prefix` when the product set one
///   (e.g. `bduff/` → `bduff/exec_<id>`) and `boss/` otherwise. This is the
///   knob exposed by `boss product … --worker-branch-prefix`; the execution
///   id is kept verbatim so the branch is unique per execution by
///   construction.
/// - [`BranchNaming::OpaqueHash`]: `boss/<sha256(execution_id)[..8]>` —
///   omits the execution id from the branch name while remaining unique
///   within a repo (32 bits of hash space).
/// - [`BranchNaming::CustomPrefix`]: `<prefix>/<sha256(execution_id)[..8]>` —
///   user-supplied prefix instead of `boss/`, same opaque hash suffix.
///
/// `worker_branch_prefix` only affects the default `BossExecPrefix`
/// strategy: a non-default `branch_naming` is the richer, explicitly
/// configured editorial rule and takes precedence over the plain prefix
/// column. The two knobs also differ in slash convention —
/// `worker_branch_prefix` already carries its trailing `/` (it is
/// concatenated verbatim), whereas `CustomPrefix { prefix }` inserts a `/`.
///
/// In every strategy the branch name is derived deterministically from
/// `execution_id` (and the frozen prefix) so the detector can reconstruct
/// it from `state.db` alone — no local jj reads, no shared-store
/// contamination.
///
/// See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for
/// the uniqueness rationale. Cross-repo hash collisions (R6) are not
/// collisions: the `gh pr list --head` query is always scoped to the
/// product's `repo_remote_url`.
pub fn expected_branch_name(
    execution_id: &str,
    branch_naming: &BranchNaming,
    worker_branch_prefix: Option<&str>,
) -> String {
    match branch_naming {
        BranchNaming::BossExecPrefix => {
            let prefix = worker_branch_prefix.unwrap_or(DEFAULT_WORKER_BRANCH_PREFIX);
            format!("{prefix}{execution_id}")
        }
        BranchNaming::OpaqueHash => {
            let hash = opaque_hash(execution_id);
            format!("boss/{hash}")
        }
        BranchNaming::CustomPrefix { prefix } => {
            let hash = opaque_hash(execution_id);
            format!("{prefix}/{hash}")
        }
    }
}

/// First 8 hex characters of the SHA-256 digest of `execution_id`.
/// Used by [`BranchNaming::OpaqueHash`] and [`BranchNaming::CustomPrefix`]
/// to build a short, unique-by-construction branch suffix that does not
/// leak the literal execution id into the branch name.
fn opaque_hash(execution_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(execution_id.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The work-item-identifying suffix of a branch name: everything after
/// the final `/` (or the whole string when there is no `/`).
///
/// Every engine-supplied branch name has the shape
/// `<prefix>/<work-item-suffix>` (see [`expected_branch_name`]), where the
/// suffix is what uniquely binds the branch to one execution — the
/// `exec_<id>` under [`BranchNaming::BossExecPrefix`], or the opaque hash
/// under [`BranchNaming::OpaqueHash`] / [`BranchNaming::CustomPrefix`].
/// The prefix (`boss/`, `bduff/`, …) is cosmetic and product-configurable
/// (`worker_branch_prefix`), so PR↔work-item association keys on the
/// suffix, never on the prefix.
pub(crate) fn branch_work_item_suffix(branch: &str) -> &str {
    branch.rsplit('/').next().unwrap_or(branch)
}

/// Whether two branch names identify the same work item, **ignoring their
/// prefixes**. A worker that honours a product's `worker_branch_prefix`
/// (e.g. `bduff/`) opens its PR on `bduff/<suffix>` while the engine
/// reconstructs `boss/<suffix>` as the expected branch; those must
/// associate so the worker is not forced to abandon a compliant PR and
/// recreate it under `boss/` (see issue #1145).
///
/// The work-item suffix is unique per execution within a repo (it is the
/// execution id or a hash of it), so matching on the suffix alone is just
/// as safe against cross-execution mis-binding as the exact-branch match
/// it replaces — a sibling worker's branch cannot share this execution's
/// suffix. An empty suffix never matches (defensive: a malformed `…/`
/// branch must not collide with another).
pub(crate) fn branches_identify_same_work_item(a: &str, b: &str) -> bool {
    let suffix_a = branch_work_item_suffix(a);
    !suffix_a.is_empty() && suffix_a == branch_work_item_suffix(b)
}

/// Probes GitHub for the PR opened against an engine-supplied branch
/// name and reports whether the PR is open / merged / closed / absent.
///
/// `repo_remote_url` is the product's `git@github.com:owner/repo.git`
/// (or `https://...`) URL — the detector parses it into an
/// `owner/repo` slug used to scope the `gh pr list` query.
/// `expected_branch` is the engine-supplied head branch (see
/// [`expected_branch_name`]).
#[async_trait]
pub trait PrDetector: Send + Sync {
    /// Returns the PR status for `expected_branch` in `repo_remote_url`.
    /// Implementations must treat "no PR with this head" as
    /// `Ok(PrStatus::None)` to keep the caller's idle-vs-completed
    /// logic clean. Errors are reserved for tool failures (`gh` auth
    /// broken, network blips, etc.).
    async fn detect_pr(
        &self,
        repo_remote_url: &str,
        expected_branch: &str,
    ) -> Result<PrStatus>;
}

/// `PrDetector` that shells out to `gh pr list --head <branch>`. The
/// branch name is engine-supplied and execution-unique
/// (see [`expected_branch_name`]), so GitHub returns at most one PR
/// per query — there is no cross-execution overlap to exploit.
///
/// Replaces the pre-incident-001 SHA-keyed recipe
/// (`jj_candidate_commit_shas` + `gh api commits/{sha}/pulls`), which
/// was structurally unsafe under cube's shared `.jj/repo/store/git`:
/// any concurrent worker's bookmark passed the revset's
/// `committer_date(after:…)` gate and the detector misattributed PRs.
#[derive(Debug, Default)]
pub struct CommandPrDetector;

impl CommandPrDetector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PrDetector for CommandPrDetector {
    async fn detect_pr(
        &self,
        repo_remote_url: &str,
        expected_branch: &str,
    ) -> Result<PrStatus> {
        let repo_slug = parse_repo_slug(repo_remote_url).with_context(|| {
            format!("failed to parse repo slug from `{repo_remote_url}`")
        })?;
        let api_pr = match query_pr_for_branch(&repo_slug, expected_branch).await? {
            Some(pr) => pr,
            None => {
                // Prefix-agnostic fallback (issue #1145): the worker may
                // have honoured a product `worker_branch_prefix` and
                // pushed its PR to `<prefix>/<suffix>` (e.g. `bduff/…`)
                // rather than the engine-reconstructed `boss/<suffix>`.
                // The exact `--head` query above misses that PR, so
                // re-query by the work-item suffix (unique per execution
                // within the repo) and accept any prefix.
                let suffix = branch_work_item_suffix(expected_branch);
                match query_pr_by_branch_suffix(&repo_slug, suffix).await? {
                    Some(pr) => {
                        tracing::info!(
                            repo = %repo_slug,
                            expected_branch = %expected_branch,
                            pr_url = %pr.url,
                            "pr_detect: no PR on the exact expected branch, but found one whose work-item suffix matches under a different prefix; associating (prefix-agnostic match)",
                        );
                        pr
                    }
                    None => {
                        tracing::debug!(
                            repo = %repo_slug,
                            branch = %expected_branch,
                            "pr_detect: no PR found for expected branch (exact or suffix match); returning None",
                        );
                        return Ok(PrStatus::None);
                    }
                }
            }
        };
        let status = classify_pr(api_pr);
        // EmptyDiff is tentative: GitHub computes diff stats
        // asynchronously, so a freshly-pushed branch can report all
        // three stat fields as 0 before the computation finishes. Run
        // a secondary check against the full PR endpoint before
        // surfacing EmptyDiff — a false positive here would loop the
        // worker pane with bogus "your diff is empty" directives on
        // every Stop event.
        if let PrStatus::EmptyDiff { ref url } = status {
            tracing::debug!(
                pr_url = %url,
                repo = %repo_slug,
                "all diff stats zero on initial check; verifying via PR endpoint",
            );
            match verify_pr_diff_nonempty(&repo_slug, url).await {
                Ok(true) => {
                    tracing::debug!(
                        pr_url = %url,
                        "secondary check confirms non-empty diff; classifying as Fresh",
                    );
                    return Ok(PrStatus::Fresh { url: url.clone() });
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(
                        pr_url = %url,
                        ?err,
                        "secondary diff-stat check failed; surfacing as detector failure",
                    );
                    return Err(err);
                }
            }
        }
        Ok(status)
    }
}

/// Fetches the `headRefName` of a PR by number, used as the Layer-2
/// defence-in-depth check before a staged PR URL drives the in_review
/// transition. Decoupled from `PrDetector` so tests can stub the two
/// concerns independently.
#[async_trait]
pub trait BranchVerifier: Send + Sync {
    /// Returns the `headRefName` for PR `pr_number` in `repo_slug`, or
    /// an error on network / API failure.
    async fn fetch_pr_head_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String>;

    /// Returns the `headRefOid` (commit SHA of the PR's head ref) for
    /// PR `pr_number` in `repo_slug`. Used by the Stop-boundary
    /// SHA-delta gate to decide whether a resume run actually moved
    /// the chore's bound PR before falling through to the
    /// `PROBE_NO_PR` nudge.
    async fn fetch_pr_head_oid(&self, repo_slug: &str, pr_number: u64) -> Result<String>;

    /// Returns the total number of changed lines (additions + deletions)
    /// between `base` and `head` in `repo_slug`. Used by the no-op /
    /// trivial-diff skip gate (P992 design §8) to detect pure rebases and
    /// trivially-small pushes that don't warrant a fresh reviewer pass.
    async fn fetch_diff_line_count(
        &self,
        repo_slug: &str,
        base: &str,
        head: &str,
    ) -> Result<u64>;

    /// Returns the description/body of PR `pr_number` in `repo_slug`.
    /// Used by the metadata-only CI-fix finalize gate (issue #1252) to
    /// detect an operator-visible PR-metadata delta: a CI-fix revision
    /// that repairs a PR-description validator via `gh pr edit --body`
    /// makes no commit, so the head SHA never moves — the body diff is
    /// the only evidence the worker contributed. An empty body is a
    /// valid value (not an error), unlike the head-ref fetches.
    async fn fetch_pr_body(&self, repo_slug: &str, pr_number: u64) -> Result<String>;
}

/// `BranchVerifier` that shells out to `gh pr view`.
#[derive(Debug, Default)]
pub struct CommandBranchVerifier;

impl CommandBranchVerifier {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BranchVerifier for CommandBranchVerifier {
    async fn fetch_pr_head_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        fetch_pr_head_ref_cmd(repo_slug, pr_number).await
    }

    async fn fetch_pr_head_oid(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        boss_github::gh_cli::fetch_pr_head_sha(repo_slug, pr_number).await
    }

    async fn fetch_diff_line_count(
        &self,
        repo_slug: &str,
        base: &str,
        head: &str,
    ) -> Result<u64> {
        fetch_diff_line_count_cmd(repo_slug, base, head).await
    }

    async fn fetch_pr_body(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        fetch_pr_body_cmd(repo_slug, pr_number).await
    }
}

/// Spawn a `gh` subprocess with the standard stdio / kill-on-drop
/// settings used throughout this module, returning the trimmed stdout on
/// success. `display` is a human-readable rendering of the command and is
/// reused in both the spawn-failure context and the non-zero-exit error
/// message (which also carries the captured stderr).
async fn run_gh(args: &[&str], display: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `{display}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{display}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Shell out to `gh pr view <pr_number> -R <repo_slug> --json headRefName`
/// and return the branch name, or an error on failure / empty response.
async fn fetch_pr_head_ref_cmd(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let head_ref = run_gh(
        &[
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefName",
            "--jq",
            ".headRefName",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug}"),
    )
    .await?;
    if head_ref.is_empty() {
        return Err(anyhow!("empty headRefName for PR {pr_number} in {repo_slug}"));
    }
    Ok(head_ref)
}


/// Shell out to `gh api repos/<repo_slug>/compare/<base>...<head>` and return
/// the total number of changed lines (additions + deletions) across all files
/// in the comparison. Returns `0` when the diff is empty (pure rebase with no
/// file-content changes). Used by the no-op skip gate (P992 design §8).
async fn fetch_diff_line_count_cmd(repo_slug: &str, base: &str, head: &str) -> Result<u64> {
    let endpoint = format!("repos/{repo_slug}/compare/{base}...{head}");
    let stdout = run_gh(
        &[
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            "(.files // []) | map(.additions + .deletions) | add // 0",
        ],
        &format!("gh api {endpoint}"),
    )
    .await?;
    let total: u64 = stdout.trim().parse().with_context(|| {
        format!("unexpected output from `gh api {endpoint}`: {:?}", stdout.trim())
    })?;
    Ok(total)
}

/// Shell out to `gh pr view <pr_number> -R <repo_slug> --json body` and
/// return the PR description. An empty body is a valid result (returned
/// as the empty string) — a PR can legitimately have no description, and
/// the metadata-fix gate needs to distinguish "" (snapshotted empty)
/// from a failed fetch.
async fn fetch_pr_body_cmd(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    run_gh(
        &[
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "body",
            "--jq",
            ".body",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug} --json body"),
    )
    .await
}


/// Parse the PR number from a canonical GitHub PR URL
/// (`https://github.com/<owner>/<repo>/pull/<N>`).
pub(crate) fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    pr_url.split('/').next_back()?.parse().ok()
}

/// Single PR row returned from `gh pr list --head <branch> --json …`.
#[derive(Debug, Clone)]
struct ApiPr {
    url: String,
    state: String,
    merged_at: Option<String>,
    /// Number of files changed in the PR.
    /// May be 0 when GitHub hasn't finished computing diff stats yet (race
    /// condition on a freshly-pushed branch); check `additions`/`deletions`
    /// before treating zero as "genuinely empty".
    changed_files: i64,
    /// Lines added in the PR.  0 means absent or not-yet-computed.
    additions: i64,
    /// Lines deleted in the PR.  0 means absent or not-yet-computed.
    deletions: i64,
}

/// Parse the first six tab-separated fields emitted by the shared
/// `gh pr list … --json url,state,mergedAt,changedFiles,additions,deletions
/// --jq … @tsv` query (in that exact order) into an [`ApiPr`].
///
/// Returns `None` when the URL field is empty — the `select(.)` /
/// row-absent case — matching the original `url.is_empty()` guard at both
/// call sites. `mergedAt` of empty or `"null"` (case-insensitively) maps to
/// `None`; the three numeric fields fall back to `0` when missing or
/// unparseable.
///
/// Any trailing fields beyond the first six (e.g. the `headRefName` column
/// in the suffix-scan query) are ignored, so callers that need them must
/// parse them separately from the same line.
fn parse_api_pr_tsv(line: &str) -> Option<ApiPr> {
    let mut parts = line.split('\t');
    let url = parts.next().unwrap_or("").trim().to_owned();
    let state = parts.next().unwrap_or("").trim().to_owned();
    let merged_at_raw = parts.next().unwrap_or("").trim();
    let changed_files_raw = parts.next().unwrap_or("0").trim();
    let additions_raw = parts.next().unwrap_or("0").trim();
    let deletions_raw = parts.next().unwrap_or("0").trim();
    if url.is_empty() {
        return None;
    }
    let merged_at = if merged_at_raw.is_empty() || merged_at_raw.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(merged_at_raw.to_owned())
    };
    Some(ApiPr {
        url,
        state,
        merged_at,
        changed_files: changed_files_raw.parse::<i64>().unwrap_or(0),
        additions: additions_raw.parse::<i64>().unwrap_or(0),
        deletions: deletions_raw.parse::<i64>().unwrap_or(0),
    })
}

fn classify_pr(pr: ApiPr) -> PrStatus {
    // Branch-keyed query already guarantees the PR was opened against
    // this execution's engine-supplied head branch — no SHA matching
    // needed. (Pre-incident-001 the detector ran a SHA-keyed query and
    // had to gate on `head.sha` matching a local commit to reject the
    // squash-merge-on-`main` misbind; branch-keyed detection makes that
    // gate structurally unnecessary because a sibling worker's
    // bookmark cannot share this execution's branch name.)
    if pr.merged_at.is_some() {
        return PrStatus::Merged { url: pr.url };
    }
    if pr.state.eq_ignore_ascii_case("closed") {
        return PrStatus::Closed { url: pr.url };
    }
    // OPEN. A PR has real changes if ANY of the three diff-stat fields
    // is positive.  `changed_files` alone is unreliable: GitHub computes
    // it asynchronously and `gh pr list` can return 0 for a freshly-pushed
    // branch before the computation finishes.  `additions` and `deletions`
    // are populated by the same pipeline but are often available sooner.
    // If ALL three are zero the PR is tentatively empty; `detect_pr` runs
    // a secondary verification call against the full PR endpoint before
    // surfacing `EmptyDiff` to callers.
    let has_changes = pr.changed_files > 0 || pr.additions > 0 || pr.deletions > 0;
    if has_changes {
        PrStatus::Fresh { url: pr.url }
    } else {
        PrStatus::EmptyDiff { url: pr.url }
    }
}

/// `gh pr list -R <slug> --head <branch> --state all` — return the
/// single PR for `branch`, or `Ok(None)` if no PR exists with that
/// head in `repo_slug`. `Err(_)` is reserved for tool / network failures.
///
/// `gh pr list --head` returns at most one open PR (GitHub enforces a
/// unique open PR per head branch), and historical closed/merged PRs
/// for the same head are extremely unlikely in practice because each
/// execution gets a unique branch name. We pass `--limit 1` defensively
/// — if multiple historical rows happen to exist, we want the most
/// recent (which `gh pr list` returns first).
async fn query_pr_for_branch(repo_slug: &str, branch: &str) -> Result<Option<ApiPr>> {
    let stdout = run_gh(
        &[
            "pr",
            "list",
            "-R",
            repo_slug,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "url,state,mergedAt,changedFiles,additions,deletions",
            "--jq",
            r#".[0] | select(.) | [(.url // ""), (.state // ""), (.mergedAt // ""), ((.changedFiles // 0) | tostring), ((.additions // 0) | tostring), ((.deletions // 0) | tostring)] | @tsv"#,
        ],
        &format!("gh pr list -R {repo_slug} --head {branch}"),
    )
    .await?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(parse_api_pr_tsv(trimmed))
}

/// Prefix-agnostic cold-path fallback (issue #1145): find a PR whose head
/// branch *ends in* `suffix` — i.e. `<any-prefix>/<suffix>` — in
/// `repo_slug`. Used when the exact `--head <boss/suffix>` query in
/// [`query_pr_for_branch`] finds nothing because the worker honoured a
/// product `worker_branch_prefix` and opened its PR under a different
/// prefix.
///
/// `gh pr list --head` only matches a full branch name, so there is no
/// server-side suffix filter; we list candidate PRs and filter in Rust by
/// [`branch_work_item_suffix`]. The work-item suffix is unique per
/// execution within a repo (the execution id or a hash of it), so at most
/// one PR can match — this preserves the incident-001 cross-execution
/// safety property (the query is still scoped to the product's repo and
/// keyed on an execution-unique token, never a shared SHA).
///
/// We scan open PRs first (the freshly-opened PR we are racing to
/// associate is open), bounded by `--limit`. If the page fills without a
/// match we emit a `warn!` rather than silently giving up, so a truncated
/// scan is visible.
async fn query_pr_by_branch_suffix(repo_slug: &str, suffix: &str) -> Result<Option<ApiPr>> {
    if suffix.is_empty() {
        return Ok(None);
    }
    const SCAN_LIMIT: usize = 100;
    let stdout = run_gh(
        &[
            "pr",
            "list",
            "-R",
            repo_slug,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "url,state,mergedAt,changedFiles,additions,deletions,headRefName",
            "--jq",
            r#".[] | [(.url // ""), (.state // ""), (.mergedAt // ""), ((.changedFiles // 0) | tostring), ((.additions // 0) | tostring), ((.deletions // 0) | tostring), (.headRefName // "")] | @tsv"#,
        ],
        &format!("gh pr list -R {repo_slug} --state all (suffix scan)"),
    )
    .await?;
    let mut rows = 0usize;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows += 1;
        // The shared 6-field parser ignores trailing columns, so pull the
        // 7th `headRefName` field out separately for the suffix filter.
        let head_ref = line.split('\t').nth(6).unwrap_or("").trim();
        if head_ref.is_empty() {
            continue;
        }
        if branch_work_item_suffix(head_ref) != suffix {
            continue;
        }
        if let Some(pr) = parse_api_pr_tsv(line) {
            return Ok(Some(pr));
        }
    }
    if rows >= SCAN_LIMIT {
        tracing::warn!(
            repo = %repo_slug,
            suffix,
            scanned = rows,
            "pr_detect: suffix scan hit the {SCAN_LIMIT}-PR limit without a match; a PR on a non-`boss/` prefix may exist beyond the scanned page",
        );
    }
    Ok(None)
}

/// Secondary diff-stat verification via the full PR endpoint.
///
/// The `commits/{sha}/pulls` response can report `changed_files == 0`
/// (and likewise `additions`/`deletions`) before GitHub finishes its
/// async diff computation on a freshly pushed branch.  This function
/// queries the authoritative per-PR endpoint and returns `true` when
/// the PR has at least one added or deleted line, so callers can
/// override an ambiguous `EmptyDiff` classification with `Fresh`.
///
/// An `Err` here means the secondary check itself failed (network blip,
/// `gh` auth issue, etc.). Callers must propagate this as a detector
/// failure rather than treating it as confirmation of an empty diff.
async fn verify_pr_diff_nonempty(repo_slug: &str, pr_url: &str) -> Result<bool> {
    let pr_number = pr_url
        .split('/')
        .next_back()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("cannot parse PR number from URL: {pr_url}"))?;
    let endpoint = format!("repos/{repo_slug}/pulls/{pr_number}");
    let stdout = run_gh(
        &[
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            "((.additions // 0) + (.deletions // 0))",
        ],
        &format!("gh api {endpoint}"),
    )
    .await?;
    let total: i64 = stdout.trim().parse().with_context(|| {
        format!("unexpected output from `gh api {endpoint}`: {:?}", stdout.trim())
    })?;
    Ok(total > 0)
}

/// Pull `owner/repo` out of a remote URL. Handles both SSH
/// (`git@github.com:owner/repo.git`) and HTTPS
/// (`https://github.com/owner/repo[.git]`) shapes.
pub(crate) fn parse_repo_slug(remote_url: &str) -> Result<String> {
    let (owner, repo) = boss_github::repo_slug::parse_github_owner_repo(remote_url)?;
    Ok(format!("{owner}/{repo}"))
}

/// Queues an automatic probe for `run_id`. The shape mirrors
/// `ServerState::queue_probe` but is exposed via a trait so the
/// completion handler can be unit-tested without standing up the full
/// app server. Implementations must be cheap and infallible — probes
/// that can't be delivered are dropped silently at injection time
/// (see `dispatch_probe_on_stop` in `app.rs`).
pub trait ProbeQueuer: Send + Sync {
    /// Push `text` onto the FIFO of probes for `run_id`. The next
    /// `Stop` event for the run pops one and `SendToPane`'s it as if
    /// the human had typed it.
    fn queue_probe(&self, run_id: &str, text: &str);
}

/// `ProbeQueuer` that drops everything — used when the test harness
/// doesn't need to assert on probe wiring.
#[derive(Debug, Default)]
pub struct NoopProbeQueuer;

impl ProbeQueuer for NoopProbeQueuer {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
}

/// Orchestrates the on-Stop completion flow: detect PR, transition
/// state in the work DB, release the cube lease, publish the right
/// invalidation events. Stateless — keeps the wiring side at the call
/// site (`app.rs`) thin.
pub struct WorkerCompletionHandler {
    work_db: Arc<WorkDb>,
    pr_detector: Arc<dyn PrDetector>,
    cube_client: Arc<dyn CubeClient>,
    publisher: Arc<dyn ExecutionPublisher>,
    pane_releaser: Arc<dyn WorkerPaneReleaser>,
    probe_queuer: Arc<dyn ProbeQueuer>,
    /// Primary-path PR URL staging. The events-socket dispatcher in
    /// `app.rs` populates this from `PostToolUse` Bash hook events
    /// whose `tool_response.stdout` carries a `gh pr create` (or
    /// `gh pr view` / `gh pr edit`) URL. When `on_stop` /
    /// `recheck_for_pr` fires, peek this cache first: if a URL is
    /// staged we trust it verbatim (`PrStatus::Fresh`) and skip the
    /// `jj log` + `gh api commits/{sha}/pulls` reconstruction
    /// entirely. Reconstruction stays as the cold-path fallback for
    /// engine-restart recovery (the cache lives in memory only).
    ///
    /// Defaults to an empty cache so test sites that don't exercise
    /// the staging path get the same behaviour they always had —
    /// nothing is staged → fall through to `pr_detector`.
    staged_pr_urls: Arc<crate::pr_url_capture::StagedPrUrlCache>,
    /// Toggleable feature flags (incident 001 AI #5). Consulted by
    /// `on_stop_inner` and `recheck_for_pr` to decide whether the
    /// cold-path PR fallback is permitted to run. Defaults to a
    /// store whose only state is the registry defaults — tests that
    /// don't wire one in get the historical behaviour
    /// (`detect_pr_cold_fallback` defaults ON).
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Layer-2 defence-in-depth verifier. Before a staged PR URL drives
    /// the in_review transition, the verifier fetches the PR's
    /// `headRefName` and confirms it matches this execution's expected
    /// branch (`boss/<execution_id>`). A mismatch means the URL was
    /// staged from an unrelated Bash invocation — it is dropped and the
    /// cold-path detector runs instead.
    ///
    /// Defaults to `CommandBranchVerifier` (shells out to `gh pr view`).
    /// Tests that exercise the staged-URL path must wire in a stub via
    /// [`Self::with_branch_verifier`] to avoid live network calls.
    branch_verifier: Arc<dyn BranchVerifier>,
    /// Engine-wide counter registry. Defaults to a fresh local registry
    /// with the PR-capture counters pre-registered so tests that do not
    /// call `with_metrics` still get valid increments. Production wires
    /// in the shared engine registry via `with_metrics` after construction.
    metrics: Arc<Registry>,
    /// GitHub probe used for the on-transition CI-status pre-fetch.
    /// When a task moves to Review the handler spawns a background task
    /// that probes the new PR's CI state so the UI card has a real icon
    /// from the first poll rather than waiting for the merge-poller sweep.
    /// Defaults to [`NoopMergeProbe`]; production wires in the shared
    /// [`CommandMergeProbe`] via [`Self::with_merge_probe`].
    merge_probe: Arc<dyn MergeProbe>,
    /// Circuit breaker for the auto-nudge loop. Every nudge site routes
    /// through [`Self::nudge_or_park`], which records the nudge against
    /// this breaker; once `max_unproductive_nudges` consecutive nudges
    /// fire with no state change the execution is parked instead of
    /// nudged again (the Worf-incident fix). Shared via `Arc` so the
    /// per-execution counters survive across the multiple `on_stop`
    /// calls of a single worker session. Defaults to a fresh breaker.
    nudge_breaker: Arc<NudgeBreaker>,
    /// Cap on consecutive unproductive auto-nudges before the breaker
    /// trips. Defaults to [`DEFAULT_MAX_UNPRODUCTIVE_NUDGES`]; tests
    /// override it via [`Self::with_max_unproductive_nudges`].
    max_unproductive_nudges: u32,
    /// Maximum number of automated reviewer passes per PR (P992 design §7).
    /// When a producing task's `review_cycle` reaches this value the engine
    /// skips the next reviewer pass and advances to human Review directly.
    /// Defaults to [`crate::config::DEFAULT_MAX_REVIEW_CYCLES`]; production
    /// wires in the value from `WorkConfig` via
    /// [`Self::with_max_review_cycles`].
    max_review_cycles: usize,
    /// Minimum changed-line count required to trigger a reviewer pass when
    /// `last_reviewed_sha` is set (P992 design §8). Pushes whose effective
    /// diff (new head vs. last-reviewed head) totals fewer lines than this
    /// threshold are skipped as trivial. Zero (the conservative default)
    /// means skip only when the diff is literally empty (pure rebase with
    /// no file-content changes). Production wires in the value from
    /// `WorkConfig` via [`Self::with_min_review_changed_lines`].
    min_review_changed_lines: u64,
    /// PR state checker passed to `create_revision` in
    /// `finalize_pr_review_pass`. Defaults to [`GhPrStateChecker`] (shells
    /// out to `gh pr view`); tests inject `FakePrStateChecker::always(Open)`
    /// via [`Self::with_pr_state_checker`] to avoid live network calls.
    pr_state_checker: Arc<dyn crate::work::PrStateChecker>,
}

impl WorkerCompletionHandler {
    pub fn new(
        work_db: Arc<WorkDb>,
        pr_detector: Arc<dyn PrDetector>,
        cube_client: Arc<dyn CubeClient>,
        publisher: Arc<dyn ExecutionPublisher>,
        pane_releaser: Arc<dyn WorkerPaneReleaser>,
        probe_queuer: Arc<dyn ProbeQueuer>,
    ) -> Self {
        // Build a local registry for tests that never call `with_metrics`.
        // Pre-register the PR-capture handles so `.inc()` never panics on
        // "counter not registered" in a test context.
        let local_metrics = Arc::new(Registry::new());
        register_metrics(&local_metrics);
        Self {
            work_db,
            pr_detector,
            cube_client,
            publisher,
            pane_releaser,
            probe_queuer,
            staged_pr_urls: Arc::new(crate::pr_url_capture::StagedPrUrlCache::new()),
            feature_flags: Arc::new(crate::feature_flags::FeatureFlagsStore::new(
                std::path::PathBuf::new(),
            )),
            branch_verifier: Arc::new(CommandBranchVerifier::new()),
            metrics: local_metrics,
            merge_probe: Arc::new(NoopMergeProbe),
            nudge_breaker: Arc::new(NudgeBreaker::new()),
            max_unproductive_nudges: DEFAULT_MAX_UNPRODUCTIVE_NUDGES,
            max_review_cycles: crate::config::DEFAULT_MAX_REVIEW_CYCLES,
            min_review_changed_lines: crate::config::DEFAULT_MIN_REVIEW_CHANGED_LINES,
            pr_state_checker: Arc::new(crate::work::GhPrStateChecker),
        }
    }

    /// Wire an externally-owned [`NudgeBreaker`] into this handler.
    /// `app.rs` does not need to call this — each handler owns its own
    /// breaker. Tests use it to share / inspect breaker state.
    pub fn with_nudge_breaker(mut self, breaker: Arc<NudgeBreaker>) -> Self {
        self.nudge_breaker = breaker;
        self
    }

    /// Override the consecutive-unproductive-nudge cap. Tests set this
    /// low to trip the breaker deterministically; production uses the
    /// default.
    pub fn with_max_unproductive_nudges(mut self, max: u32) -> Self {
        self.max_unproductive_nudges = max;
        self
    }

    /// Override the automated-reviewer cycle cap (P992 design §7).
    /// Production wires in `WorkConfig.max_review_cycles` via `app.rs`;
    /// tests that need to exercise the cycle-bound path set it low.
    pub fn with_max_review_cycles(mut self, max: usize) -> Self {
        self.max_review_cycles = max;
        self
    }

    /// Override the trivial-diff skip threshold (P992 design §8).
    /// Production wires in `WorkConfig.min_review_changed_lines` via `app.rs`;
    /// tests that exercise the trivial-diff path set it to a small value.
    pub fn with_min_review_changed_lines(mut self, min: u64) -> Self {
        self.min_review_changed_lines = min;
        self
    }

    /// Override the PR state checker used by `finalize_pr_review_pass` when
    /// creating a revision (P992 task 8). Tests inject
    /// `FakePrStateChecker::always(Open)` to avoid live `gh` calls.
    #[cfg(test)]
    fn with_pr_state_checker(
        mut self,
        checker: Arc<dyn crate::work::PrStateChecker>,
    ) -> Self {
        self.pr_state_checker = checker;
        self
    }

    /// Wire the engine-global metrics registry into this handler. `app.rs`
    /// calls this once after `init_all` has registered the PR-capture
    /// counter handles. Tests that omit this call use a pre-seeded local
    /// registry (created in `new`) so counter increments never panic.
    pub fn with_metrics(mut self, metrics: Arc<Registry>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Wire an externally-owned [`StagedPrUrlCache`] into this
    /// handler so the events-socket dispatcher and the on-Stop
    /// resolver share the same map. `app.rs` calls this once after
    /// construction; tests that want to exercise the staged-URL
    /// path can call it with their own cache. Tests that don't
    /// invoke it get the default empty cache from `new` and follow
    /// the legacy detector path — preserving the pre-change
    /// behaviour without a signature break.
    pub fn with_staged_pr_urls(
        mut self,
        cache: Arc<crate::pr_url_capture::StagedPrUrlCache>,
    ) -> Self {
        self.staged_pr_urls = cache;
        self
    }

    /// Wire an externally-owned [`BranchVerifier`] into this handler
    /// for Layer-2 staged-URL branch validation. `app.rs` does not need
    /// to call this — the default `CommandBranchVerifier` is correct for
    /// production. Tests that exercise the staged-URL path must call
    /// this with a stub to avoid live `gh pr view` calls.
    pub fn with_branch_verifier(mut self, verifier: Arc<dyn BranchVerifier>) -> Self {
        self.branch_verifier = verifier;
        self
    }

    /// Wire an externally-owned [`FeatureFlagsStore`] into this
    /// handler so engine-wide flag toggles are observed by the
    /// completion path. `app.rs` calls this once at startup with the
    /// store loaded from `~/Library/Application Support/Boss/feature-flags.toml`.
    /// Tests that don't invoke it get the default store (every flag
    /// at its registry default), preserving the pre-change behaviour.
    pub fn with_feature_flags(
        mut self,
        flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    ) -> Self {
        self.feature_flags = flags;
        self
    }

    /// Wire the shared [`MergeProbe`] for the on-transition CI pre-fetch.
    /// `app.rs` passes the same [`CommandMergeProbe`] used by the merge
    /// poller so both paths share probe logic. Tests that do not need the
    /// CI-fetch path can omit this call and rely on the default
    /// [`NoopMergeProbe`].
    pub fn with_merge_probe(mut self, probe: Arc<dyn MergeProbe>) -> Self {
        self.merge_probe = probe;
        self
    }

    /// Wire an externally-owned [`StagedResolutionSignalCache`] into this
    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        let outcome = self.on_stop_inner(execution_id).await;
        // `ci_remediation` (retrigger-kind only; fix-kind now dispatches through
        // revision_implementation) gets the catch-all finalizer on Stop.
        if let Ok(execution) = self.work_db.get_execution(execution_id)
            && execution.kind == ExecutionKind::CiRemediation {
                self.finalize_ci_remediation_attempt(&execution, &outcome)
                    .await;
            }
        outcome
    }

    async fn on_stop_inner(&self, execution_id: &str) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "stop event: execution unknown — likely a non-execution worker run"
                );
                return StopOutcome::UnknownExecution;
            }
        };

        // Already completed/failed/cancelled — nothing more to do.
        if !execution.status.is_live() {
            return StopOutcome::AlreadyTerminal;
        }

        // Stale-Stop guard (reused-workspace hook leak): if a newer live
        // execution now occupies this execution's cube workspace, this
        // Stop leaked from a stale `boss-event` hook registration left in
        // the warm-cached workspace. Finalizing here would mis-attribute
        // completion to the wrong run and could release the live run's
        // re-leased workspace. Ignore it; the newest execution's own Stop
        // drives its completion. Belt-and-suspenders with
        // `worker_setup::purge_leaked_worker_hooks`, which stops the leak
        // at the source.
        match self.work_db.execution_superseded_in_workspace(&execution) {
            Ok(true) => {
                tracing::warn!(
                    execution_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "stop event: execution superseded by a newer live execution in the same reused workspace — ignoring stale Stop (reused-workspace hook leak)",
                );
                return StopOutcome::SupersededInWorkspace;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "stop event: superseded-in-workspace check failed; proceeding with completion",
                );
            }
        }

        // Maint task 6: an `automation_triage` execution never opens a PR.
        // Its Stop is resolved by the marker-protocol outcome detector
        // (`automation: task <id>` / `automation: skip — …`), not by PR
        // detection or the nudge path below. Branch out before any of that.
        if execution.kind == ExecutionKind::AutomationTriage {
            return self.finalize_automation_triage(&execution).await;
        }

        // P992: a `pr_review` reviewer execution never opens a PR. It reads
        // the PR diff and emits structured findings; the producing task already
        // advanced to `in_review` on PR-open, so the Stop handler just finalises
        // the reviewer execution (task 8 will also parse the ReviewResult and
        // enqueue revisions when warranted).
        if execution.kind == ExecutionKind::PrReview {
            return self.finalize_pr_review_pass(&execution).await;
        }

        // Flaky/infra retrigger park (issue #1205): a `ci_remediation`
        // worker that diagnosed the CI failure as infra and re-ran the job
        // (`mark-retriggered`) stamped the `ci_flaky_retriggered` signal on
        // the parent. There is nothing to push, so we MUST NOT fall through
        // to PR detection or the nudge loop — every probe would just
        // re-derive the same verdict and burn worker turns. Park the worker
        // awaiting the CI retry / a human decision. The merge-poller clears
        // the signal and snaps the parent to Review once CI goes green.
        if execution.kind == ExecutionKind::CiRemediation {
            match self
                .work_db
                .has_active_ci_flaky_retrigger_signal(&execution.work_item_id)
            {
                Ok(true) => {
                    let pr_url = self.resolve_bound_pr_url(&execution).unwrap_or_default();
                    tracing::info!(
                        execution_id,
                        work_item_id = %execution.work_item_id,
                        %pr_url,
                        "stop event: parent carries ci_flaky_retriggered signal — parking worker (awaiting CI retry / human decision), not nudging",
                    );
                    return StopOutcome::FlakyRetriggered { pr_url };
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        work_item_id = %execution.work_item_id,
                        ?err,
                        "stop event: flaky-retrigger signal check failed; proceeding with normal completion",
                    );
                }
            }
        }

        // Primary path: a PR URL was already captured from a
        // `PostToolUse` Bash hook event (`gh pr create` /
        // `gh pr view` / `gh pr edit` stdout) while the worker was
        // still running. Layer-2 defence-in-depth: verify the staged
        // PR's headRefName matches this execution's expected branch
        // before finalizing — a mismatch means the URL was captured
        // from an unrelated Bash invocation and must be discarded.
        //
        // The cold-path fallback below remains for engine-restart
        // recovery: if the engine was down when the worker ran
        // `gh pr create`, the in-memory staging cache is empty here
        // and we fall through to `detect_pr` to reconstruct the URL
        // via the GitHub API.
        if let Some(staged_url) = self.staged_pr_urls.get(execution_id) {
            let expected_branch = expected_branch_name(
                execution_id,
                &execution.branch_naming,
                execution.worker_branch_prefix.as_deref(),
            );
            let repo_slug = parse_repo_slug(&execution.repo_remote_url);
            let branch_ok = match repo_slug {
                Ok(ref slug) => {
                    match pr_number_from_url(&staged_url) {
                        Some(pr_num) => {
                            match self.branch_verifier.fetch_pr_head_ref(slug, pr_num).await {
                                Ok(ref head_ref)
                                    if branches_identify_same_work_item(
                                        head_ref,
                                        &expected_branch,
                                    ) =>
                                {
                                    if head_ref.as_str() != expected_branch.as_str() {
                                        tracing::info!(
                                            execution_id,
                                            staged_pr_url = %staged_url,
                                            staged_pr_branch = %head_ref,
                                            %expected_branch,
                                            "stop event: staged PR branch prefix differs from expected but the work-item suffix matches; associating (prefix-agnostic match)",
                                        );
                                    }
                                    true
                                }
                                Ok(head_ref) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        staged_pr_branch = %head_ref,
                                        %expected_branch,
                                        "pr_recheck_staged_branch_mismatch: staged PR work-item suffix does not match expected; dropping staged URL",
                                    );
                                    PR_RECHECK_STAGED_BRANCH_MISMATCH.inc(&self.metrics);
                                    self.staged_pr_urls.forget(execution_id);
                                    false
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        ?err,
                                        "stop event: branch verification failed; dropping staged URL for safety",
                                    );
                                    self.staged_pr_urls.forget(execution_id);
                                    false
                                }
                            }
                        }
                        None => {
                            tracing::warn!(
                                execution_id,
                                staged_pr_url = %staged_url,
                                "stop event: cannot parse PR number from staged URL; dropping for safety",
                            );
                            self.staged_pr_urls.forget(execution_id);
                            false
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "stop event: cannot parse repo slug; dropping staged URL for safety",
                    );
                    self.staged_pr_urls.forget(execution_id);
                    false
                }
            };
            if branch_ok {
                tracing::info!(
                    execution_id,
                    pr_url = %staged_url,
                    "stop event: using PR URL captured from worker hook stream (primary path); skipping detector",
                );
                PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
                return self
                    .finalize_pr_transition(
                        execution_id,
                        staged_url,
                        WorkerPrCompletionTarget::InReview,
                        "stop_staged",
                    )
                    .await;
            }
        }

        // AI #6 running-status gate (incident 001 §5): in Claude Code
        // the `Stop` hook fires after every assistant turn, not just
        // at worker exit. With no staged URL on a still-`running`
        // execution we MUST NOT fall through to `detect_pr` — the
        // worker is alive and any positive result would race against
        // its own in-flight push. Reserve the fallback for
        // genuinely-terminal worker sessions, which in the engine's
        // execution lifecycle are stamped `waiting_human` (set by
        // `finish_execution_run` after `PaneSpawnRunner` returns).
        if execution.status != ExecutionStatus::WaitingHuman {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                "stop event: no staged URL and execution is not waiting_human — skipping fallback (running-status gate)",
            );
            return StopOutcome::RunningNoStagedPr;
        }

        // Resume-bounce SHA-delta gate: when the chore already has a
        // PR bound to it (`task.pr_url` populated by an earlier run's
        // on-Stop machinery), use that URL as the authoritative
        // identifier — never branch-search. If the bound PR's head
        // SHA moved during this run (vs the snapshot captured at run
        // start in `execution.pr_head_before`), the worker
        // contributed and we should finalize without nudging. If the
        // PR did not move, queue the nudge directly and skip the
        // cold-path detector entirely (its branch-keyed search would
        // miss the bound PR on a resume, producing the false-positive
        // nudge loop this gate exists to prevent).
        match self.evaluate_sha_delta_gate(execution_id, &execution).await {
            ShaDeltaGateOutcome::Contributed { pr_url } => {
                return self
                    .finalize_pr_transition(
                        execution_id,
                        pr_url,
                        WorkerPrCompletionTarget::InReview,
                        "stop_sha_delta",
                    )
                    .await;
            }
            ShaDeltaGateOutcome::NoContribution { pr_url } => {
                // Before nudging, check whether the blocking signal (conflict /
                // CI) is already cleared — e.g. a sibling resolver fixed the
                // conflict before this run started. If so, retire the attempt
                // and finalise the execution without nudging.
                if let Some(outcome) = self
                    .try_retire_cleared_blocking_signal(execution_id, &execution, &pr_url)
                    .await
                {
                    return outcome;
                }
                // Positive-evidence metadata-only CI-fix gate (issue #1252):
                // a revision can legitimately finish WITHOUT moving the head
                // when it repairs a PR-description validator via
                // `gh pr edit --body` (no commit). Because we are inside the
                // on-Stop handler, this is a *real* Stop boundary — a dead /
                // cut-off worker emits no Stop hook and never reaches here.
                // If this run also produced an operator-visible PR-metadata
                // delta, record that positive evidence and finalize (now, if
                // CI is already green; otherwise the merge poller finalizes
                // it once CI goes green — see `recheck_for_pr`). Without a
                // delta we fall through to the normal nudge: head unchanged
                // AND body unchanged means the worker contributed nothing.
                if let Some(outcome) = self
                    .try_finalize_metadata_only_fix_on_stop(execution_id, &execution, &pr_url)
                    .await
                {
                    return outcome;
                }
                tracing::info!(
                    execution_id,
                    bound_pr_url = %pr_url,
                    "stop event: bound PR did not move during this run — nudging to push to the existing PR"
                );
                // A PR is already bound: never tell the worker to create
                // one. Nudge it to push to the existing branch, bounded
                // by the circuit breaker.
                return self
                    .nudge_or_park(
                        &execution,
                        &probe_push_to_existing_pr(&pr_url),
                        &format!("nocontribution:{pr_url}"),
                        Some(&pr_url),
                        StopOutcome::AwaitingInput,
                    )
                    .await;
            }
            ShaDeltaGateOutcome::Inapplicable => {
                // `revision_implementation` executions with a captured
                // `pr_head_before` snapshot: the gate returned Inapplicable
                // because the GitHub API fetch failed transiently, not because
                // no baseline exists. The cold-path branch-keyed detector
                // always returns None for revisions (they push commits to the
                // parent PR's branch and never open their own PR), so the only
                // cold-path outcome is: bound PR found via
                // resolve_bound_pr_url → nudge "push to existing PR". That
                // nudge loops: each Stop re-triggers the same probe (T939
                // regression: Crusher was stuck in waiting_for_input). Return
                // AwaitingInput silently instead; the merge poller's
                // recheck_for_pr will finalize via the SHA-delta gate once the
                // API recovers.
                if execution.kind == ExecutionKind::RevisionImplementation
                    && execution
                        .pr_head_before
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .is_some()
                    && let Some(bound_pr_url) = self.resolve_bound_pr_url(&execution) {
                        tracing::info!(
                            execution_id,
                            %bound_pr_url,
                            "stop event: revision_implementation with pr_head_before set but \
                             SHA-delta fetch failed — skipping cold-path nudge to avoid \
                             probe loop; recheck_for_pr will finalize when API recovers"
                        );
                        return StopOutcome::AwaitingInput;
                    }
                // No bound `chore.pr_url`, or the snapshot/fetch was
                // unavailable. Fall through to the existing
                // branch-keyed cold-path detector (new-PR flow).
            }
        }

        // AI #5 feature-flag gate (incident 001 §5): the cold-path
        // fallback is the path that produced the mis-binds in the
        // incident. The human can flip this off in the macOS app
        // debug pane to immediately suppress the path without a
        // rebuild. When OFF, empty staging falls through to "no PR
        // pushed" — the chore stays in `waiting_human` until the
        // human resolves it by hand.
        if !self.feature_flags.is_enabled("detect_pr_cold_fallback") {
            tracing::info!(
                execution_id,
                "stop event: detect_pr_cold_fallback flag is OFF — skipping fallback",
            );
            return StopOutcome::FallbackDisabledByFlag;
        }

        let expected_branch = expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        PR_URL_CAPTURE_RECONSTRUCTION_HIT.inc(&self.metrics);
        let pr_status = match self
            .pr_detector
            .detect_pr(&execution.repo_remote_url, &expected_branch)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                // Do NOT probe the worker on a detector failure.  The failure
                // is usually a transient `gh`/network issue; probing here
                // creates a re-entrancy loop: worker receives the probe,
                // responds, stops, detection fails again, probe again…
                // The merge-poller's recheck sweep will recover the
                // transition once the failure clears.
                tracing::warn!(
                    execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "stop event: PR detection failed; will retry on next merge-poller sweep"
                );
                PR_URL_CAPTURE_RECONSTRUCTION_FAILED.inc(&self.metrics);
                return StopOutcome::DetectorFailed;
            }
        };

        let (pr_url, target) = match pr_status {
            PrStatus::None | PrStatus::Closed { .. } => {
                // The branch-keyed detector found no PR on *this*
                // execution's branch. Before concluding "no PR, nudge to
                // create one", resolve whether the chore already has a
                // PR bound on a sibling execution (the `ci_remediation`
                // / resume case the cold-path search structurally
                // misses). If so, never say `gh pr create` — nudge to
                // push to the existing PR instead.
                if let Some(bound_pr_url) = self.resolve_bound_pr_url(&execution) {
                    tracing::info!(
                        execution_id,
                        expected_branch = %expected_branch,
                        %bound_pr_url,
                        kind = %execution.kind,
                        "stop event: chore already has a bound PR the branch search missed — nudging to push to it, not create"
                    );
                    return self
                        .nudge_or_park(
                            &execution,
                            &probe_push_to_existing_pr(&bound_pr_url),
                            &format!("push_existing:{bound_pr_url}"),
                            Some(&bound_pr_url),
                            StopOutcome::AwaitingInput,
                        )
                        .await;
                }
                // No bound PR resolvable. A `ci_remediation` worker must
                // NEVER be told to create a PR — if it somehow has no
                // bound PR, that is an anomalous upstream state; park it
                // for a human rather than nudging it to `gh pr create`.
                if execution.kind == ExecutionKind::CiRemediation {
                    tracing::warn!(
                        execution_id,
                        kind = %execution.kind,
                        "stop event: ci_remediation execution has no resolvable bound PR — parking instead of nudging to create one"
                    );
                    return self
                        .park_for_unproductive_nudges(
                            &execution,
                            0,
                            None,
                            "ci_remediation execution has no bound PR to push to; it must not be \
asked to open one",
                        )
                        .await;
                }
                // `revision_implementation` workers must NEVER be told to
                // create a PR — their deliverable is a commit on the parent
                // task's existing PR branch.  The chain-root lookup above
                // covers the common case; if we still have no resolvable PR
                // it is an upstream data anomaly.  Park for a human instead
                // of contradicting the worker's own task instructions.
                if execution.kind == ExecutionKind::RevisionImplementation {
                    tracing::warn!(
                        execution_id,
                        kind = %execution.kind,
                        "stop event: revision_implementation execution has no resolvable bound PR — parking instead of nudging to create one"
                    );
                    return self
                        .park_for_unproductive_nudges(
                            &execution,
                            0,
                            None,
                            "revision_implementation execution has no bound PR to push to; it \
must not be asked to open one",
                        )
                        .await;
                }
                tracing::info!(
                    execution_id,
                    expected_branch = %expected_branch,
                    "stop event: worker idle without an active PR — probing to push and open one"
                );
                return self
                    .nudge_or_park(
                        &execution,
                        PROBE_NO_PR,
                        "no_pr",
                        None,
                        StopOutcome::AwaitingInput,
                    )
                    .await;
            }
            PrStatus::Stale { url, reason } => {
                tracing::info!(
                    execution_id,
                    expected_branch = %expected_branch,
                    pr_url = %url,
                    %reason,
                    "stop event: PR exists but local commits are unpushed — probing to push"
                );
                return self
                    .nudge_or_park(
                        &execution,
                        PROBE_STALE_PR,
                        &format!("stale:{url}"),
                        Some(&url),
                        StopOutcome::StalePr {
                            pr_url: url.clone(),
                            reason,
                        },
                    )
                    .await;
            }
            PrStatus::EmptyDiff { url } => {
                tracing::warn!(
                    execution_id,
                    expected_branch = %expected_branch,
                    pr_url = %url,
                    "stop event: PR has an empty diff — worker pushed a no-op change; probing to fix or close"
                );
                return self
                    .nudge_or_park(
                        &execution,
                        PROBE_EMPTY_PR,
                        &format!("empty:{url}"),
                        Some(&url),
                        StopOutcome::EmptyDiffPr {
                            pr_url: url.clone(),
                        },
                    )
                    .await;
            }
            PrStatus::Fresh { url } => (url, WorkerPrCompletionTarget::InReview),
            PrStatus::Merged { url } => (url, WorkerPrCompletionTarget::Done),
        };
        self.finalize_pr_transition(execution_id, pr_url, target, "stop")
            .await
    }

    /// Periodic fallback for the merge poller. Re-runs PR detection
    /// against `execution_id` and transitions the work item on a
    /// `Fresh` / `Merged` result, but stays QUIET on the no-PR /
    /// stale-PR / detector-failure branches — the on-Stop probe
    /// queueing and `worker_awaiting_pr` publish only make sense as a
    /// one-shot response to a Stop event. A 60s poller calling
    /// `on_stop` would (a) spam the worker's probe FIFO with
    /// duplicate "push your branch" messages every minute and
    /// (b) publish a steady stream of `worker_awaiting_pr` events
    /// while the worker sat idle. `recheck_for_pr` exists so the
    /// poller can drive the success path without the side effects.
    ///
    /// Closes the missed-PR-open window: if the on-Stop hook fired
    /// before GitHub's `commits/{sha}/pulls` index caught up with a
    /// freshly-created PR (the typical 7-second window observed in
    /// PR #415), this sweep picks the chore up on the next pass and
    /// completes the `active → in_review` transition.
    pub async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(_) => return StopOutcome::UnknownExecution,
        };
        if !execution.status.is_live() {
            return StopOutcome::AlreadyTerminal;
        }
        // P992 task 7: reviewer executions never open a PR; skip the
        // PR-detection recheck entirely. The reviewer's Stop path already
        // drives its resolution via finalize_pr_review_pass.
        if execution.kind == ExecutionKind::PrReview {
            return StopOutcome::AlreadyTerminal;
        }
        // Primary path mirror: if the PostToolUse dispatcher already
        // captured this execution's PR URL from the worker's hook
        // stream, finalize via that URL and skip the detector. Layer-2
        // defence-in-depth: verify the staged PR's headRefName matches
        // this execution's expected branch before trusting the URL. A
        // mismatch means the URL was captured from an unrelated Bash
        // invocation (e.g. reading a chore description that referenced
        // an old PR number) and must be discarded.
        if let Some(staged_url) = self.staged_pr_urls.get(execution_id) {
            let expected_branch = expected_branch_name(
                execution_id,
                &execution.branch_naming,
                execution.worker_branch_prefix.as_deref(),
            );
            let repo_slug = parse_repo_slug(&execution.repo_remote_url);
            let branch_ok = match repo_slug {
                Ok(ref slug) => {
                    match pr_number_from_url(&staged_url) {
                        Some(pr_num) => {
                            match self.branch_verifier.fetch_pr_head_ref(slug, pr_num).await {
                                Ok(ref head_ref)
                                    if branches_identify_same_work_item(
                                        head_ref,
                                        &expected_branch,
                                    ) =>
                                {
                                    if head_ref.as_str() != expected_branch.as_str() {
                                        tracing::info!(
                                            execution_id,
                                            staged_pr_url = %staged_url,
                                            staged_pr_branch = %head_ref,
                                            %expected_branch,
                                            "pr-recheck: staged PR branch prefix differs from expected but the work-item suffix matches; associating (prefix-agnostic match)",
                                        );
                                    }
                                    true
                                }
                                Ok(head_ref) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        staged_pr_branch = %head_ref,
                                        %expected_branch,
                                        "pr_recheck_staged_branch_mismatch: staged PR work-item suffix does not match expected; dropping staged URL",
                                    );
                                    PR_RECHECK_STAGED_BRANCH_MISMATCH.inc(&self.metrics);
                                    self.staged_pr_urls.forget(execution_id);
                                    false
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        ?err,
                                        "pr-recheck: branch verification failed; dropping staged URL for safety",
                                    );
                                    self.staged_pr_urls.forget(execution_id);
                                    false
                                }
                            }
                        }
                        None => {
                            tracing::warn!(
                                execution_id,
                                staged_pr_url = %staged_url,
                                "pr-recheck: cannot parse PR number from staged URL; dropping for safety",
                            );
                            self.staged_pr_urls.forget(execution_id);
                            false
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "pr-recheck: cannot parse repo slug; dropping staged URL for safety",
                    );
                    self.staged_pr_urls.forget(execution_id);
                    false
                }
            };
            if branch_ok {
                tracing::info!(
                    execution_id,
                    pr_url = %staged_url,
                    "pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector",
                );
                PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
                return self
                    .finalize_pr_transition(
                        execution_id,
                        staged_url,
                        WorkerPrCompletionTarget::InReview,
                        "pr_recheck_staged",
                    )
                    .await;
            }
        }

        // Running-status gate mirror (AI #6): the merge-poller's
        // recheck sweep is intended for `waiting_human` workers whose
        // staged URL was missed. Skipping for `running` keeps the
        // fallback off in-flight workers even when the poller's
        // candidate query picks them up by race.
        if execution.status != ExecutionStatus::WaitingHuman {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                "pr-recheck: skipping fallback — execution is not waiting_human (running-status gate)",
            );
            return StopOutcome::RunningNoStagedPr;
        }

        // SHA-delta gate: for executions with a bound PR URL — either the
        // task's own `pr_url` (chore resume) or `execution.pr_url` for
        // `revision_implementation` tasks (which never open their own PR but
        // push commits to the parent PR) — check whether the bound PR's HEAD
        // SHA moved since this execution started.
        //
        // This is the primary recovery path for `revision_implementation`
        // executions: the cold-path branch-keyed detector always returns None
        // for revisions because they have no branch of their own. The SHA-delta
        // gate is therefore the only fallback that can advance a revision from
        // `active` to `in_review` when `on_stop_inner` failed transiently (e.g.
        // a GitHub API timeout during the SHA fetch, or an engine restart
        // between execution start and Stop).  Without this gate the merge-poller
        // sweep repeatedly calls `recheck_for_pr`, finds no PR on the revision's
        // branch, and silently returns — leaving the revision stranded in `doing`
        // even after the worker successfully pushed its commit (reproduces T848).
        //
        // `Contributed` → finalize now (worker pushed to the bound PR).
        // `NoContribution` / `Inapplicable` → fall through; the cold path
        // returns quietly for revisions (no PR on their own branch).
        match self.evaluate_sha_delta_gate(execution_id, &execution).await {
            ShaDeltaGateOutcome::Contributed { pr_url } => {
                tracing::info!(
                    execution_id,
                    pr_url = %pr_url,
                    "pr-recheck: SHA-delta gate: bound PR head moved — finalising without cold-path detector",
                );
                return self
                    .finalize_pr_transition(
                        execution_id,
                        pr_url,
                        WorkerPrCompletionTarget::InReview,
                        "pr_recheck_sha_delta",
                    )
                    .await;
            }
            ShaDeltaGateOutcome::NoContribution { pr_url } => {
                // Bound PR did not advance during this run. For most resumes
                // the cold-path detector below returns quietly for revisions
                // and the next sweep retries, waiting for a push that moves
                // the head.
                //
                // The one exception is a legitimate PR-metadata-only CI fix
                // (issue #1252): a revision that repaired a PR-description
                // validator with `gh pr edit --body` makes no commit, so the
                // head never moves and CI can go green *after* the worker
                // stopped — past the last Stop event, so `on_stop` can no
                // longer finalize it. The merge poller is the only path that
                // can. But — unlike the rolled-back #1262 gate — we do NOT
                // infer "done" from "head unchanged + CI green": that race
                // reaped live and dead workers alike. We finalize ONLY when
                // `on_stop` already stamped the positive-evidence marker
                // (a real Stop boundary observed an operator-visible PR-body
                // delta) AND CI is now green. A dead/cut-off worker never
                // reaches a clean Stop, so it never carries the marker and is
                // never finalized here — it falls through and is surfaced /
                // re-dispatched by the normal incomplete-execution paths.
                if execution.kind == ExecutionKind::RevisionImplementation
                    && self
                        .work_db
                        .execution_metadata_fix_confirmed(execution_id)
                        .unwrap_or(false)
                    && let Some(outcome) = self
                        .finalize_metadata_only_revision_if_ready(execution_id, &pr_url)
                        .await
                {
                    return outcome;
                }
            }
            ShaDeltaGateOutcome::Inapplicable => {
                // No bound PR or snapshot unavailable — fall through to the
                // cold-path branch-keyed detector.
            }
        }

        // Feature-flag gate mirror (AI #5): the merge-poller's sweep
        // runs on the same cold-path fallback `on_stop_inner` does,
        // so the human's debug-pane toggle must take effect here too.
        if !self.feature_flags.is_enabled("detect_pr_cold_fallback") {
            tracing::debug!(
                execution_id,
                "pr-recheck: detect_pr_cold_fallback flag is OFF — skipping fallback",
            );
            return StopOutcome::FallbackDisabledByFlag;
        }

        let expected_branch = expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        PR_URL_CAPTURE_RECONSTRUCTION_HIT.inc(&self.metrics);
        let pr_status = match self
            .pr_detector
            .detect_pr(&execution.repo_remote_url, &expected_branch)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "pr-recheck: detector failed; will retry next sweep"
                );
                PR_URL_CAPTURE_RECONSTRUCTION_FAILED.inc(&self.metrics);
                return StopOutcome::DetectorFailed;
            }
        };
        let (pr_url, target) = match pr_status {
            // Quiet returns — no probes, no awaiting-input publish.
            PrStatus::None | PrStatus::Closed { .. } => return StopOutcome::AwaitingInput,
            PrStatus::Stale { url, reason } => {
                return StopOutcome::StalePr { pr_url: url, reason }
            }
            PrStatus::EmptyDiff { url } => return StopOutcome::EmptyDiffPr { pr_url: url },
            PrStatus::Fresh { url } => (url, WorkerPrCompletionTarget::InReview),
            PrStatus::Merged { url } => (url, WorkerPrCompletionTarget::Done),
        };
        self.finalize_pr_transition(execution_id, pr_url, target, "pr_recheck")
            .await
    }

    /// PR-detection recheck for a terminal execution (status
    /// `abandoned`, `completed`, or `failed`) whose task is still
    /// `active` with no `pr_url`. This is the Bug B recovery path for
    /// the double-spawn race: exec_A is abandoned by the orphan sweep,
    /// exec_A's pane later pushes a PR, and the on-Stop hook returns
    /// `AlreadyTerminal` because exec_A is already in a terminal status.
    ///
    /// Unlike [`Self::recheck_for_pr`] this method does **not** gate on
    /// execution status and does **not** call
    /// `record_worker_pr_completion` (which requires `running` /
    /// `waiting_human`). Instead, on a `Fresh` PR detection, it calls
    /// [`WorkDb::bind_pr_to_active_task_from_terminal_execution`] to
    /// advance only the task row.
    pub async fn recheck_for_pr_late(
        &self,
        candidate: &crate::work::LatePrCandidate,
    ) -> StopOutcome {
        let expected_branch = expected_branch_name(
            &candidate.execution_id,
            &candidate.branch_naming,
            candidate.worker_branch_prefix.as_deref(),
        );
        let pr_status = match self
            .pr_detector
            .detect_pr(&candidate.repo_remote_url, &expected_branch)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(
                    execution_id = %candidate.execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "pr-recheck-late: detector failed; will retry next sweep"
                );
                return StopOutcome::DetectorFailed;
            }
        };
        let pr_url = match pr_status {
            PrStatus::None | PrStatus::Closed { .. } => return StopOutcome::AwaitingInput,
            PrStatus::Stale { url, reason } => {
                return StopOutcome::StalePr { pr_url: url, reason }
            }
            PrStatus::EmptyDiff { url } => return StopOutcome::EmptyDiffPr { pr_url: url },
            PrStatus::Fresh { url } | PrStatus::Merged { url } => url,
        };
        match self
            .work_db
            .bind_pr_to_active_task_from_terminal_execution(&candidate.work_item_id, &pr_url)
        {
            Ok(true) => {
                tracing::info!(
                    execution_id = %candidate.execution_id,
                    work_item_id = %candidate.work_item_id,
                    pr_url = %pr_url,
                    "pr-recheck-late: bound late PR to active task (double-spawn recovery)",
                );
                StopOutcome::PrDetected { pr_url }
            }
            Ok(false) => StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %candidate.execution_id,
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "pr-recheck-late: DB update failed"
                );
                StopOutcome::DbError
            }
        }
    }

    /// Common Fresh/Merged transition path shared by `on_stop_inner`
    /// and `recheck_for_pr`. Records the completion, releases the
    /// cube lease + pane, publishes invalidation events, and returns
    /// the matching [`StopOutcome`]. `source` distinguishes call
    /// sites in the publish reason and tracing — `"stop"` for the
    /// Maint task 6: resolve a finished `automation_triage` execution via the
    /// marker protocol and finalise both its `automation_runs` row and the
    /// execution itself.
    ///
    /// The worker was told to end its final message with exactly one of
    /// `automation: task <id>` or `automation: skip — <reason>`. Steps:
    /// 1. read the final assistant message and parse the decision;
    /// 2. for a `task` marker, verify the id resolves to a task carrying this
    ///    automation's provenance — so a misbehaving agent can't pass off an
    ///    unrelated task as its own output;
    /// 3. record the terminal outcome (`produced_task` / `skipped`, or keep
    ///    `failed_will_retry` for a missing / ambiguous / unverifiable marker);
    /// 4. finalise the execution (`completed`) and release pane + workspace.
    async fn finalize_automation_triage(
        &self,
        execution: &crate::work::WorkExecution,
    ) -> StopOutcome {
        let automation_id = execution.work_item_id.clone();
        let transcript = self.read_final_triage_message(&execution.id).await;
        let decision = match &transcript {
            TriageTranscript::FinalMessage(text) => parse_triage_decision(text),
            // No path / unreadable / no assistant prose all mean we have no
            // message to scan for a marker — treat as NoDecision, but the
            // specific transcript state is folded into the detail below so the
            // run history distinguishes "ran but emitted no marker" from
            // "produced no transcript at all".
            TriageTranscript::NoPath
            | TriageTranscript::Unreadable
            | TriageTranscript::NoAssistantText => TriageDecision::NoDecision,
        };

        let (outcome, produced_task_id, detail): (&str, Option<String>, Option<String>) =
            match &decision {
                TriageDecision::ProducedTask(marker_id) => {
                    match self.work_db.get_work_item_resolving_short_id(marker_id) {
                        Ok(Some(WorkItem::Task(t))) | Ok(Some(WorkItem::Chore(t)))
                            if t.source_automation_id.as_deref()
                                == Some(automation_id.as_str()) =>
                        {
                            // Explicit success detail (not `None`): it overwrites
                            // the pessimistic dispatch-time placeholder so a row
                            // that still reads "dispatched; awaiting …" can only
                            // mean the worker never reached Stop (crashed/hung).
                            (
                                AUTOMATION_OUTCOME_PRODUCED_TASK,
                                Some(t.id.clone()),
                                Some(format!("produced task {}", t.id)),
                            )
                        }
                        other => {
                            tracing::warn!(
                                execution_id = %execution.id,
                                automation_id = %automation_id,
                                marker_id,
                                resolved_some = ?other.as_ref().map(|o| o.is_some()),
                                "triage emitted a task marker but no task with this automation's \
                                 provenance matched; leaving run failed_will_retry",
                            );
                            (
                                AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                                None,
                                Some(format!(
                                    "triage emitted `automation: task {marker_id}` but no task \
                                     with this automation's provenance was found"
                                )),
                            )
                        }
                    }
                }
                TriageDecision::Skip(reason) => {
                    let reason = if reason.is_empty() {
                        "no reason given".to_owned()
                    } else {
                        reason.clone()
                    };
                    (AUTOMATION_OUTCOME_SKIPPED, None, Some(reason))
                }
                TriageDecision::NoDecision => (
                    AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                    None,
                    Some(triage_no_decision_detail(&transcript)),
                ),
                TriageDecision::Ambiguous(n) => (
                    AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                    None,
                    Some(format!(
                        "triage emitted {n} decision markers; expected exactly one"
                    )),
                ),
            };

        match self.work_db.finalize_automation_triage_run(
            &execution.id,
            outcome,
            produced_task_id.as_deref(),
            detail.as_deref(),
        ) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                execution_id = %execution.id,
                automation_id = %automation_id,
                "no automation_runs row matched this triage execution; outcome not recorded",
            ),
            Err(err) => tracing::error!(
                execution_id = %execution.id,
                ?err,
                "failed to finalise automation_runs row for triage execution",
            ),
        }

        // Finalise the execution + release pane and cube workspace, mirroring
        // the PR-completion finalizer's release order. Capture the lease id
        // before `finish_execution_run` nulls the lease columns.
        let lease_id = execution.cube_lease_id.clone();
        match self.work_db.active_run_ids_for_execution(&execution.id) {
            Ok(run_ids) => {
                for run_id in run_ids {
                    if let Err(err) = self.work_db.finish_execution_run(
                        &execution.id,
                        &run_id,
                        ExecutionStatus::Completed,
                        "completed",
                        Some(&format!("automation triage: {outcome}")),
                        None,
                        /* clear_workspace_lease */ true,
                        None,
                    ) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            run_id,
                            ?err,
                            "failed to finish triage execution run",
                        );
                    }
                }
            }
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "failed to list active runs for triage finalisation",
            ),
        }
        if let Some(lease_id) = lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id = %execution.id,
                    lease_id,
                    ?err,
                    "triage finalisation: cube workspace release failed",
                );
            }
        self.pane_releaser.release_pane(&execution.id).await;
        self.publisher
            .publish(
                &execution.id,
                &automation_id,
                "completed",
                "automation_triage_completed",
            )
            .await;

        tracing::info!(
            execution_id = %execution.id,
            automation_id = %automation_id,
            outcome,
            produced_task_id = ?produced_task_id,
            detail = ?detail,
            "automation triage finalised",
        );
        StopOutcome::AutomationTriage {
            outcome: outcome.to_owned(),
        }
    }

    /// P992 task 8: finalise a `pr_review` reviewer execution when its Stop
    /// hook fires. The reviewer never opens a PR; instead, it reads the
    /// producing task's PR diff and emits structured `ReviewResult` JSON in
    /// a fenced code block in its final message. This handler:
    ///
    /// 1. Reads the reviewer's final assistant message from its transcript.
    /// 2. Extracts and parses the `ReviewResult` JSON block.
    /// 3. Applies the engine severity gate (design §3): any `critical`/`high`
    ///    finding, or any `regression` finding (regardless of severity), warrants
    ///    a revision. `revision_warranted = false` alone does not suppress the gate.
    ///    4a. If the gate passes: creates a revision task on the producing task
    ///    with the rendered findings as `revision_instructions`, `source =
    ///    pr_review`, dispatched on the general worker pool (`autostart = true`).
    ///    The producing task advances from `active` → `in_review` at this point;
    ///    the revision is an additional follow-up child task.
    ///    4b. If the gate does not pass (no qualifying findings, or no parseable
    ///    `ReviewResult`): the producing task advances to `in_review`.
    ///
    /// Until this handler fires, the producing task is held in `active` (Doing)
    /// with `pr_url` stamped and `ai_reviewing = true` in the derived work-tree
    /// projection. A fallback sweep in the merge poller ensures the hold always
    /// resolves even if this Stop never arrives.
    ///
    /// In either case the reviewer execution is completed and its workspace
    /// released — it is always terminal after this handler runs.
    async fn finalize_pr_review_pass(
        &self,
        execution: &crate::work::WorkExecution,
    ) -> StopOutcome {
        let producing_task_id = &execution.work_item_id;

        // Look up the producing task to retrieve its pr_url (stamped during
        // the PendingReview write when the reviewer was enqueued).
        let pr_url = match self.work_db.get_work_item(producing_task_id) {
            Ok(WorkItem::Task(ref t)) | Ok(WorkItem::Chore(ref t)) => {
                match t.pr_url.as_deref() {
                    Some(url) if !url.is_empty() => url.to_owned(),
                    _ => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            producing_task_id,
                            "pr_review finalize: producing task has no pr_url; \
                             cannot advance to in_review",
                        );
                        return StopOutcome::DbError;
                    }
                }
            }
            Ok(other) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    producing_task_id,
                    item_type = ?other,
                    "pr_review finalize: work_item_id does not resolve to a task/chore",
                );
                return StopOutcome::DbError;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    producing_task_id,
                    ?err,
                    "pr_review finalize: could not load producing task",
                );
                return StopOutcome::DbError;
            }
        };

        // P992 task 8: parse ReviewResult from the reviewer's transcript and
        // apply the severity gate. Falls back gracefully (no revision) when the
        // reviewer produced no parseable JSON block.
        let review_result = self.read_final_triage_message(&execution.id).await
            .into_message()
            .and_then(|text| {
                let result = crate::pr_review::extract_review_result(&text);
                if result.is_none() {
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        "pr_review finalize: no parseable ReviewResult JSON block in \
                         reviewer transcript; advancing to in_review without revision",
                    );
                }
                result
            });

        // P992 task 9: extract head_sha before review_result is (potentially)
        // consumed by the revision path below. Used to update last_reviewed_sha.
        let head_sha_for_cycle: Option<String> = review_result
            .as_ref()
            .map(|r| r.head_sha.clone())
            .filter(|s| !s.is_empty());

        let revision_warranted = review_result
            .as_ref()
            .is_some_and(crate::pr_review::passes_severity_gate);

        // Atomically: advance the producing task from active → in_review +
        // complete the reviewer execution + clear its cube columns. Same path
        // for both revision and no-revision cases.
        let completion = match self.work_db.record_worker_pr_completion(
            &execution.id,
            &pr_url,
            None,
            WorkerPrCompletionTarget::InReview,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    producing_task_id,
                    ?err,
                    "pr_review finalize: DB write failed",
                );
                return StopOutcome::DbError;
            }
        };

        // P992 task 9: increment the review cycle counter and record
        // last_reviewed_sha. This happens regardless of whether a revision
        // was warranted — the cycle ticks on every completed reviewer pass.
        // A failure here is non-fatal (the task is already in in_review).
        if let Err(err) = self
            .work_db
            .increment_task_review_cycle(producing_task_id, head_sha_for_cycle.as_deref())
        {
            tracing::warn!(
                execution_id = %execution.id,
                producing_task_id,
                ?err,
                "pr_review finalize: failed to increment review_cycle; \
                 cycle-bound enforcement may be off by one",
            );
        }

        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id = %execution.id,
                    lease_id,
                    ?err,
                    "pr_review finalize: cube workspace release failed",
                );
            }
        self.pane_releaser.release_pane(&execution.id).await;

        let product_id = work_item_product_id(&completion.work_item);
        let work_item_id = work_item_id(&completion.work_item);

        // P992 task 8: if the severity gate passed, create a revision on the
        // producing task with the rendered findings as revision instructions.
        // The revision is dispatched on the general worker pool (autostart = true,
        // the default). Nothing is posted to GitHub — feedback stays inside Boss.
        if revision_warranted {
            // `review_result` is Some when `revision_warranted` is true.
            let result = review_result.expect("revision_warranted implies Some(ReviewResult)");
            let instructions = crate::pr_review::render_revision_instructions(&result);
            let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", execution.id);

            match self.work_db.create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(producing_task_id.clone())
                    .description(instructions)
                    .created_via(created_via)
                    .build(),
                self.pr_state_checker.as_ref(),
            ) {
                Ok(revision) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        producing_task_id,
                        revision_task_id = %revision.id,
                        pr_url = %pr_url,
                        findings = result.findings.len(),
                        "pr_review pass finalised; revision created for qualifying findings",
                    );
                    self.publisher
                        .publish(
                            &execution.id,
                            &work_item_id,
                            "completed",
                            "pr_review_pass_revision_created",
                        )
                        .await;
                    self.publisher
                        .publish_work_item_changed(
                            &product_id,
                            &work_item_id,
                            "pr_review_pass_revision_created",
                        )
                        .await;
                    return StopOutcome::ReviewPassRevisionCreated {
                        pr_url,
                        revision_task_id: revision.id,
                    };
                }
                Err(err) => {
                    // Revision creation failed (parent no longer revisable — PR
                    // merged or closed between review and now). The producing task
                    // is already in in_review; fall through to ReviewPassCompleted.
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        ?err,
                        "pr_review finalize: create_revision failed (parent likely no longer \
                         revisable); advancing to in_review without revision",
                    );
                }
            }
        }

        self.publisher
            .publish(
                &execution.id,
                &work_item_id,
                "completed",
                "pr_review_pass_completed",
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "pr_review_pass_completed")
            .await;

        tracing::info!(
            execution_id = %execution.id,
            producing_task_id,
            pr_url = %pr_url,
            "pr_review pass finalised; producing task advanced to in_review",
        );
        StopOutcome::ReviewPassCompleted { pr_url }
    }

    /// Read the final assistant text of `execution_id`'s transcript, if any.
    /// Returns `None` when no transcript is recorded/readable or it contains
    /// no assistant turn — the caller treats that as "no decision".
    /// Read a finished triage execution's final assistant message from its
    /// transcript, returning a [`TriageTranscript`] that distinguishes the
    /// failure-to-read cases (no path / unreadable / no assistant prose) from a
    /// successful read. The caller folds these states into the run-history
    /// `detail` so a `failed_will_retry` triage row is diagnosable instead of
    /// collapsing to a bare "no decision marker".
    async fn read_final_triage_message(&self, execution_id: &str) -> TriageTranscript {
        let path = match self.work_db.transcript_path_for_execution(execution_id) {
            Ok(Some(path)) => path,
            Ok(None) => {
                tracing::warn!(
                    execution_id,
                    "triage finalisation: no transcript path recorded; treating as no decision",
                );
                return TriageTranscript::NoPath;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "triage finalisation: transcript lookup failed",
                );
                return TriageTranscript::Unreadable;
            }
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let events = crate::transcript_markdown::parse_transcript(&content);
                // Collect ALL assistant text turns, not just the last one.
                //
                // The triage agent emits its decision marker in the turn AFTER the
                // `boss task create` Bash call.  The Stop hook can fire before that
                // post-tool turn is fully flushed to disk, so `iter().rev().find_map`
                // (which returned only the last AssistantText) would land on the
                // pre-tool analysis message — which has no marker — and record
                // `failed_will_retry` even though the task was successfully created.
                //
                // Joining all turns mirrors `attentions_detector::extract_assistant_text`
                // and ensures the marker is found regardless of which turn contains it.
                // The "exactly one marker" contract still holds: `parse_triage_decision`
                // enforces it across the combined text.
                let all_text: Vec<String> = events
                    .iter()
                    .filter_map(|e| match &e.kind {
                        crate::transcript_markdown::TranscriptEventKind::AssistantText(t) => {
                            Some(t.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if all_text.is_empty() {
                    tracing::warn!(
                        execution_id,
                        transcript_bytes = content.len(),
                        event_count = events.len(),
                        "triage finalisation: transcript had no assistant text event",
                    );
                    TriageTranscript::NoAssistantText
                } else {
                    tracing::debug!(
                        execution_id,
                        transcript_bytes = content.len(),
                        event_count = events.len(),
                        assistant_turns = all_text.len(),
                        "triage finalisation: read all assistant turns for marker scan",
                    );
                    TriageTranscript::FinalMessage(all_text.join("\n"))
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "triage finalisation: failed to read transcript file",
                );
                TriageTranscript::Unreadable
            }
        }
    }

    /// Evaluate the no-op / trivial-diff skip gate for the automated reviewer
    /// (P992 design §8).
    ///
    /// Returns `Some(reason)` when the reviewer pass should be skipped,
    /// or `None` when a full review is warranted.
    ///
    /// Rules, in order:
    /// 1. If `review_cycle == 0` or `last_reviewed_sha` is `None` → first
    ///    review → never skip (design: "first review of a PR is never skipped
    ///    by the trivial rule").
    /// 2. If the current PR head OID equals `last_reviewed_sha` → skip
    ///    (`"sha_unchanged"`): the worker pushed the exact same commit.
    /// 3. If the effective diff between `last_reviewed_sha` and the current
    ///    head is 0 changed lines → skip (`"empty_diff"`): pure rebase with
    ///    no file-content changes.
    /// 4. If `min_review_changed_lines > 0` and the diff is below that
    ///    threshold → skip (`"trivial_diff"`): cosmetically small push.
    ///
    /// API errors during steps 2–4 are logged and treated as "don't skip"
    /// so the reviewer still runs on uncertainty.
    async fn check_noop_skip(
        &self,
        pr_url: &str,
        producing: &crate::work::WorkExecution,
        review_cycle: i64,
        last_reviewed_sha: Option<&str>,
    ) -> Option<&'static str> {
        let Some(last_sha) = last_reviewed_sha else {
            return None; // first review
        };
        if review_cycle == 0 {
            return None; // first review (belt-and-suspenders; last_sha is None when cycle=0)
        }

        // Parse repo slug and PR number for GitHub API calls.
        let repo_slug = match parse_repo_slug(&producing.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    repo_remote_url = %producing.repo_remote_url,
                    ?err,
                    "pr_review noop gate: cannot parse repo slug; proceeding with review",
                );
                return None;
            }
        };
        let Some(pr_number) = pr_number_from_url(pr_url) else {
            tracing::warn!(
                pr_url,
                "pr_review noop gate: cannot parse PR number; proceeding with review",
            );
            return None;
        };

        // Fetch current PR head SHA.
        let current_head = match self
            .branch_verifier
            .fetch_pr_head_oid(&repo_slug, pr_number)
            .await
        {
            Ok(sha) => sha,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    ?err,
                    "pr_review noop gate: cannot fetch PR head OID; proceeding with review",
                );
                return None;
            }
        };

        // Rule 2: exact SHA match — nothing changed since last review.
        if current_head == last_sha {
            return Some("sha_unchanged");
        }

        // Rules 3 & 4: compare effective diff between last-reviewed head and
        // current head. Fail open on API errors.
        let diff_lines = match self
            .branch_verifier
            .fetch_diff_line_count(&repo_slug, last_sha, &current_head)
            .await
        {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    last_reviewed_sha = last_sha,
                    current_head = %current_head,
                    ?err,
                    "pr_review noop gate: cannot fetch diff line count; proceeding with review",
                );
                return None;
            }
        };

        if diff_lines == 0 {
            return Some("empty_diff");
        }

        if self.min_review_changed_lines > 0 && diff_lines < self.min_review_changed_lines {
            return Some("trivial_diff");
        }

        None
    }

    /// Stop hook path, `"pr_recheck"` for the merge-poller's
    /// fallback sweep — so operators can see which path closed a
    /// given chore.
    async fn finalize_pr_transition(
        &self,
        execution_id: &str,
        pr_url: String,
        target: WorkerPrCompletionTarget,
        source: &'static str,
    ) -> StopOutcome {
        let merged = matches!(target, WorkerPrCompletionTarget::Done);

        // P992 tasks 7 & 9: for reviewer-triggering executions with a fresh
        // (non-merged) PR, try to enqueue an independent reviewer pass
        // instead of immediately advancing to human Review (design §1).
        // Task 9 adds: check the cycle bound first — if review_cycle has
        // already reached max_review_cycles, skip the reviewer and proceed
        // to InReview with a sticky attention item for the human.
        // If the pr_review execution cannot be created (DB error), fall back
        // to the normal InReview path so the task is never left stuck.
        let enqueued_reviewer = if !merged
            && matches!(target, WorkerPrCompletionTarget::InReview)
        {
            match self.work_db.get_execution(execution_id) {
                Ok(ref producing)
                    if should_enqueue_reviewer_for_primary(&producing.kind)
                        || (producing.kind == ExecutionKind::RevisionImplementation
                            && should_enqueue_reviewer_for_revision(
                                &producing.work_item_id,
                                &self.work_db,
                            )) =>
                {
                    // P992 tasks 9 & 10: read cycle state once — used by both
                    // the no-op gate (task 10) and the cycle-bound check (task 9).
                    let max_cycles = self.max_review_cycles;
                    let (review_cycle, last_reviewed_sha) = match self
                        .work_db
                        .get_task_review_cycle_state(&producing.work_item_id)
                    {
                        Ok(state) => state,
                        Err(err) => {
                            // Fail open: treat as cycle=0, no prior SHA so both
                            // gates pass through (don't skip on uncertainty).
                            tracing::warn!(
                                execution_id,
                                work_item_id = %producing.work_item_id,
                                ?err,
                                "could not read review_cycle; assuming bound not reached",
                            );
                            (0i64, None)
                        }
                    };

                    // P992 task 10: no-op / trivial-diff skip gate. Runs before
                    // the cycle-bound check so a pure rebase doesn't consume a
                    // cycle slot or surface an attention item.
                    let noop_skip_reason = self
                        .check_noop_skip(
                            &pr_url,
                            producing,
                            review_cycle,
                            last_reviewed_sha.as_deref(),
                        )
                        .await;

                    if let Some(skip_reason) = noop_skip_reason {
                        tracing::info!(
                            execution_id,
                            work_item_id = %producing.work_item_id,
                            skip_reason,
                            "pr_review noop skip: advancing to in_review without reviewer pass",
                        );
                        false
                    } else {
                    // P992 task 9: cycle bound check.
                    let cycle_bound_reached = (review_cycle as usize) >= max_cycles;

                    if cycle_bound_reached {
                        tracing::info!(
                            execution_id,
                            work_item_id = %producing.work_item_id,
                            max_review_cycles = max_cycles,
                            "pr_review cycle bound reached; skipping reviewer \
                             and advancing to in_review",
                        );
                        // Surface a sticky attention item so the human can see
                        // the cycle limit was hit when they open the PR card.
                        let _ = self.work_db.create_attention_item(
                            CreateAttentionItemInput {
                                work_item_id: Some(producing.work_item_id.clone()),
                                kind: "pr_review_cycle_bound".to_owned(),
                                title: format!(
                                    "Automated reviewer: cycle limit ({max_cycles}) reached"
                                ),
                                body_markdown: format!(
                                    "The automated reviewer completed {max_cycles} \
                                     cycle(s) on this PR without resolving all findings. \
                                     The PR has been advanced to human Review.\n\n\
                                     See the most recent revision task for the outstanding \
                                     findings from the last automated review cycle."
                                ),
                                execution_id: None,
                                status: None,
                                resolved_at: None,
                            },
                        );
                        false
                    } else {
                        match self.work_db.create_execution(
                            CreateExecutionInput::builder()
                                .work_item_id(producing.work_item_id.clone())
                                .kind(ExecutionKind::PrReview)
                                .status(ExecutionStatus::Ready)
                                .repo_remote_url(producing.repo_remote_url.clone())
                                .build(),
                        ) {
                            Ok(review_exec) => {
                                tracing::info!(
                                    execution_id,
                                    review_execution_id = %review_exec.id,
                                    pr_url = %pr_url,
                                    producing_kind = %producing.kind,
                                    "pr_review execution enqueued; \
                                     holding producing task for reviewer pass",
                                );
                                self.publisher.kick_scheduler();
                                true
                            }
                            Err(err) => {
                                tracing::warn!(
                                    execution_id,
                                    ?err,
                                    "failed to create pr_review execution; \
                                     falling back to immediate in_review",
                                );
                                false
                            }
                        }
                    }
                    } // closes the `} else {` for the noop skip gate
                }
                Ok(_) => false, // non-reviewer-triggering execution; advance to in_review as normal
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "could not load execution for reviewer-enqueue check; \
                         falling back to immediate in_review",
                    );
                    false
                }
            }
        } else {
            false
        };

        let effective_target = if enqueued_reviewer {
            WorkerPrCompletionTarget::PendingReview
        } else {
            target
        };

        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            None,
            effective_target,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id,
                    source,
                    ?err,
                    "pr completion: failed to record"
                );
                return StopOutcome::DbError;
            }
        };
        // Clear the staged URL now that the DB write succeeded.
        // Deliberately ordered after `record_worker_pr_completion` so
        // a failed DB write leaves the cache intact and the next
        // merge-poller sweep can retry with the same staged URL.
        self.staged_pr_urls.forget(execution_id);
        // The worker contributed a PR — reset any accumulated nudge
        // count so a later unrelated nudge cycle starts clean.
        self.nudge_breaker.forget(execution_id);
        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id,
                    source,
                    lease_id,
                    ?err,
                    "pr completion: cube release failed"
                );
            }
        self.pane_releaser.release_pane(execution_id).await;
        let product_id = work_item_product_id(&completion.work_item);
        let work_item_id = work_item_id(&completion.work_item);
        let publish_reason = match (merged, source) {
            (true, "pr_recheck") => "worker_pr_merged_recheck",
            (false, "pr_recheck") => "worker_pr_completed_recheck",
            (true, _) => "worker_pr_merged",
            (false, _) => "worker_pr_completed",
        };
        self.publisher
            .publish(
                &completion.execution.id,
                &completion.execution.work_item_id,
                completion.execution.status.as_str(),
                publish_reason,
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, publish_reason)
            .await;
        // Auto-populate the project's design-doc pointer when the
        // completed work item is a `kind=design` task with a project.
        // Errors are logged inside the detector — they must not surface
        // here because they'd mask the successful PR transition.
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item
            && task.kind == TaskKind::Design
                && let Some(ref project_id) = task.project_id {
                    if merged {
                        // Worker merged directly during its session; update
                        // the branch to main (base_ref_name unknown here,
                        // so the detector will fetch it from the PR).
                        design_detector::on_design_pr_merged(
                            &self.work_db,
                            &task.id,
                            &task.product_id,
                            project_id,
                            &pr_url,
                            None,
                        )
                        .await;
                    } else {
                        design_detector::on_design_pr_detected(
                            &self.work_db,
                            &task.id,
                            &task.product_id,
                            project_id,
                            &pr_url,
                        )
                        .await;
                    }

                    // Attentions creation pipeline (design: attentions.md).
                    // A design worker may ship a sibling `<slug>.attentions.json`
                    // question manifest; parse it off the PR branch and upsert
                    // the question group. Idempotent across re-detections.
                    if let Some((group, created)) =
                        attentions_detector::reconcile_design_doc_questions(
                            &self.work_db,
                            &task.id,
                            project_id,
                            &pr_url,
                            merged,
                        )
                        .await
                    {
                        self.publish_attentions_created(&group, &created).await;
                    }
                }

        // Followups: any completing implementation worker may emit a
        // `FOLLOWUPS:` block near the end of its run. Parse the transcript
        // tail and upsert a followup group keyed to the originating work
        // item. A no-op (no transcript / no block) when absent; idempotent
        // across re-runs via the store's content dedup.
        let transcript_path = self
            .work_db
            .transcript_path_for_execution(execution_id)
            .ok()
            .flatten();
        if let Some((group, created)) = attentions_detector::reconcile_task_followups(
            &self.work_db,
            &work_item_id,
            execution_id,
            transcript_path.as_deref(),
        )
        .await
        {
            self.publish_attentions_created(&group, &created).await;
        }

        if merged {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR already merged; moved work item to done"
            );
            StopOutcome::PrMerged { pr_url }
        } else if enqueued_reviewer {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; reviewer enqueued — \
                 producing task held in active pending review pass",
            );
            StopOutcome::ReviewerEnqueued { pr_url }
        } else {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; moved work item to in_review"
            );
            // Pre-fetch CI status so the Review card has a real icon from
            // the first frame. The fetch is fire-and-forget: if it fails or
            // the probe is slow the UI falls back to the in-progress default
            // and the merge-poller sweep picks it up on its next pass.
            let probe = self.merge_probe.clone();
            let work_db = self.work_db.clone();
            let publisher = self.publisher.clone();
            let candidate = PendingMergeCheck {
                work_item_id: work_item_id.clone(),
                product_id: product_id.clone(),
                pr_url: pr_url.clone(),
            };
            tokio::spawn(async move {
                match probe.probe(&candidate.pr_url).await {
                    Ok(lifecycle_probe) => {
                        update_pr_poll_state(
                            &work_db,
                            publisher.as_ref(),
                            &candidate,
                            &lifecycle_probe,
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "pr completion: on-transition CI pre-fetch failed; \
                             merge poller will retry on next sweep",
                        );
                    }
                }
            });
            StopOutcome::PrDetected { pr_url }
        }
    }

    /// Push an `AttentionCreated` event per newly-created member on the
    /// owning product's work-tree topic so the Notifications window and the
    /// design-doc viewer live-update (mirrors the `CreateAttention` RPC
    /// handler). No-op for an empty `created` set.
    async fn publish_attentions_created(&self, group: &AttentionGroup, created: &[Attention]) {
        for attention in created {
            self.publisher
                .publish_frontend_event_on_product(
                    &group.product_id,
                    FrontendEvent::AttentionCreated {
                        attention: attention.clone(),
                        group: group.clone(),
                    },
                )
                .await;
        }
    }

    /// Phase 10 #33: catch-all finaliser for `ci_remediation` workers.
    /// Mirrors [`Self::finalize_conflict_resolution_attempt`]. Fires for
    /// every Stop event on a `ci_remediation` execution; decides whether
    /// to mark the bound `ci_remediations` row `failed` with the
    /// catch-all reason ([`CI_NO_PUSH_REASON`]).
    ///
    /// Same rule as the conflict-resolver flow: if the attempt is still
    /// `running`, `head_sha_after IS NULL`, `failure_reason IS NULL`,
    /// AND the worker exited without pushing (PR not freshly bound),
    /// the engine has no signal that the worker classified its own
    /// outcome — default to `failed` with the catch-all reason. On
    /// `Fresh` / `Merged` outcomes the merge poller's `on_ci_resolved`
    /// retire path will mark the attempt `succeeded` shortly. On the
    /// `Stale` / `EmptyDiff` paths the on-Stop probe queue is already
    /// chasing the worker for a follow-up push, so leave the attempt
    /// alone.
    ///
    /// Idempotent — the underlying
    /// [`WorkDb::mark_ci_remediation_failed`] WHERE-guards on
    /// `status IN ('pending', 'running')`, so a duplicate finaliser
    /// call after a terminal transition writes nothing.
    pub async fn finalize_ci_remediation_attempt(
        &self,
        execution: &crate::work::WorkExecution,
        outcome: &StopOutcome,
    ) {
        let attempt = match self
            .work_db
            .active_ci_remediation_for_work_item(&execution.work_item_id)
        {
            Ok(Some(attempt)) => attempt,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    "ci-remediation finalizer: no active attempt; nothing to do",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "ci-remediation finalizer: failed to look up active attempt",
                );
                return;
            }
        };
        // Already past the "running with no outcome" window — the
        // worker classified via `mark-failed` / `mark-retriggered`,
        // the poller already retired it, or some other path closed
        // the row. Nothing for the catch-all to do.
        if attempt.status != "running"
            || attempt.head_sha_after.is_some()
            || attempt.failure_reason.is_some()
        {
            return;
        }

        let should_mark_failed = match outcome {
            // Worker pushed (or the PR is already merged from this run).
            // The merge poller's on_ci_resolved retire path will mark
            // the attempt `succeeded` once CI is green.
            StopOutcome::PrDetected { .. } | StopOutcome::PrMerged { .. } => false,
            // Worker pushed something but the PR head still trails the
            // worker's local commits, or pushed an empty diff. The
            // on-Stop probe path has already nudged the worker; don't
            // pre-empt with a `failed` mark.
            StopOutcome::StalePr { .. } | StopOutcome::EmptyDiffPr { .. } => false,
            // Race with an already-finalized execution, or a stale Stop
            // from a superseded reused-workspace occupant.
            StopOutcome::AlreadyTerminal
            | StopOutcome::UnknownExecution
            | StopOutcome::SupersededInWorkspace => false,
            // Incident-001 gates (mirrors conflict finalizer).
            StopOutcome::RunningNoStagedPr => false,
            StopOutcome::FallbackDisabledByFlag => false,
            // Breaker parked the execution for a human; don't mark the
            // attempt failed (that risks a retrigger that re-loops).
            StopOutcome::NudgeBreakerParked { .. } => false,
            // Signal was already cleared before this worker ran — the
            // attempt has been marked succeeded by try_retire_cleared_blocking_signal.
            StopOutcome::SignalAlreadyCleared { .. } => false,
            // Worker re-triggered a flaky/infra failure — the attempt was
            // already flipped to terminal `retriggered` by mark-retriggered,
            // so there is no running row to retire here, and this is
            // explicitly NOT a failure.
            StopOutcome::FlakyRetriggered { .. } => false,
            // Unreachable here (this finalizer only runs for `ci_remediation`
            // kind), but a triage outcome must never mark a CI attempt failed.
            StopOutcome::AutomationTriage { .. } => false,
            // Unreachable: reviewer executions short-circuit before CI
            // remediation finalisation. Covered for exhaustiveness.
            StopOutcome::ReviewerEnqueued { .. }
            | StopOutcome::ReviewPassCompleted { .. }
            | StopOutcome::ReviewPassRevisionCreated { .. } => false,
            // Catch-all branches: worker exited without evidence of a
            // push and without classifying via `mark-failed`.
            StopOutcome::AwaitingInput
            | StopOutcome::DetectorFailed
            | StopOutcome::NoWorkspace
            | StopOutcome::DbError => true,
        };
        if !should_mark_failed {
            return;
        }

        let updated = match self
            .work_db
            .mark_ci_remediation_failed(&attempt.id, CI_NO_PUSH_REASON)
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    "ci-remediation finalizer: attempt already terminal between probes",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "ci-remediation finalizer: failed to mark attempt failed",
                );
                return;
            }
        };

        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            attempt_id = %updated.id,
            pr_url = %updated.pr_url,
            reason = CI_NO_PUSH_REASON,
            ?outcome,
            "ci-remediation finalizer: worker exited without pushing; attempt marked failed",
        );

        self.publisher
            .publish_frontend_event_on_product(
                &updated.product_id,
                FrontendEvent::CiRemediationFailed {
                    product_id: updated.product_id.clone(),
                    work_item_id: updated.work_item_id.clone(),
                    attempt_id: updated.id.clone(),
                    pr_url: updated.pr_url.clone(),
                    failure_reason: CI_NO_PUSH_REASON.to_owned(),
                },
            )
            .await;
    }

    /// Force-release the resources backing `execution_id`: tear down
    /// the libghostty pane and release the cube workspace. Idempotent —
    /// duplicate calls (e.g. completion-detection followed by a manual
    /// stop, or two clients racing to mark a chore done) become no-ops
    /// on the second pass via the registry's `take_slot_for_run`
    /// invariant and the DB's lease-id ownership transfer.
    ///
    /// Does NOT change the execution's status field. Callers that need
    /// the execution marked `completed` / `failed` should drive that
    /// transition through the appropriate `WorkDb` method.
    pub async fn force_release(&self, execution_id: &str) {
        // Pane release first. Idempotent on the registry side; the
        // implementation logs and skips when no slot is mapped.
        //
        // The outcome gates the cube release below: only a worker whose
        // pane was actually found and reaped frees its lease. A worker
        // still mid-spawn (no slot mapped yet, no pid to reap) reports
        // `NoLiveWorker` — releasing its lease now would hand a
        // workspace it is about to occupy back to cube, which re-leases
        // it into a same-workspace collision (T981). In that case the
        // lease stays held; the in-flight `run_execution` reaps the
        // worker once its spawn settles and releases the lease then.
        if matches!(
            self.pane_releaser.release_pane(execution_id).await,
            PaneReleaseOutcome::NoLiveWorker
        ) {
            tracing::info!(
                execution_id,
                "force_release: no live worker pane mapped (mid-spawn or already released); \
                 leaving the cube lease held — the in-flight run releases it after reaping, \
                 so an occupied workspace is never re-leased",
            );
            return;
        }

        // Cube release: claim ownership of the lease id atomically by
        // clearing it from the DB row before calling the cube CLI.
        // A concurrent caller will see `None` and skip.
        let lease_id = match self.work_db.clear_execution_workspace(execution_id) {
            Ok(Some(lease_id)) => lease_id,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "force_release: failed to clear execution workspace columns",
                );
                return;
            }
        };
        if let Err(err) = self.cube_client.release_workspace(&lease_id).await {
            tracing::warn!(
                execution_id,
                lease_id,
                ?err,
                "force_release: cube workspace release failed",
            );
        }
    }

    /// Stop a worker whose task was dragged back to Backlog by the user.
    /// Cancels the execution row in the DB (so the orphan sweep and
    /// reconciler won't re-dispatch it) then releases the pane and cube
    /// workspace via `force_release`. Does NOT demote the task status —
    /// the `UpdateWorkItem` handler already applied the user's `todo`
    /// patch before this is called.
    ///
    /// `reason` names what triggered the cancel (e.g. the kanban
    /// `active → todo` drag). It is stamped on the trace record so a
    /// post-mortem can attribute *what* cancelled an execution — the
    /// gap that blocked attribution of the T981 mid-spawn cancel, where
    /// the record carried no initiator at all.
    pub async fn cancel_and_release(&self, execution_id: &str, reason: &str) {
        match self.work_db.cancel_running_execution(execution_id) {
            Ok(true) => {
                tracing::info!(
                    execution_id,
                    reason,
                    "cancel_and_release: execution cancelled",
                );
            }
            Ok(false) => {
                tracing::debug!(
                    execution_id,
                    reason,
                    "cancel_and_release: execution already terminal; proceeding to release",
                );
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    reason,
                    ?err,
                    "cancel_and_release: failed to cancel execution; proceeding to release",
                );
            }
        }
        self.force_release(execution_id).await;
    }

    /// Explicit human-initiated stop (`bossctl agents stop`). Unlike
    /// the normal `on_stop` hook path — which probes for a PR and
    /// waits for the worker to respond — this path is used when the
    /// operator wants the worker dead *now*. The differences:
    ///
    /// 1. Cancels the execution atomically in the DB (so the orphan
    ///    sweep and `reconcile_active_dispatch` don't re-dispatch the
    ///    work item the moment the pane is released and the worker
    ///    pool slot is freed).
    /// 2. Demotes the task from `active` → `todo` so the kanban
    ///    card moves back to the Backlog column instead of sitting
    ///    in Doing with no live worker.
    /// 3. Publishes a `work_item_changed` event so the UI and
    ///    downstream subscribers see the status transition.
    /// 4. Then calls `force_release` to kill the pane and free
    ///    the cube workspace.
    ///
    /// Idempotent: a second call for the same execution is a no-op
    /// at both the DB and the cube-release layers.
    pub async fn force_stop_execution(&self, execution_id: &str) {
        let (exec_cancelled, task_demoted) = match self
            .work_db
            .cancel_running_execution_and_demote_task(execution_id)
        {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "force_stop: failed to cancel execution / demote task — proceeding to release",
                );
                (false, false)
            }
        };

        if exec_cancelled || task_demoted {
            tracing::info!(
                execution_id,
                exec_cancelled,
                task_demoted,
                "force_stop: cancelled execution and demoted task",
            );
            // Publish work-item-changed so the UI refreshes. Requires
            // looking up the execution's work_item_id + product_id.
            if let Ok(execution) = self.work_db.get_execution(execution_id)
                && let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    let product_id = work_item_product_id(&work_item);
                    let wid = work_item_id(&work_item);
                    self.publisher
                        .publish_work_item_changed(&product_id, &wid, "worker_force_stopped")
                        .await;
                }
        }

        self.force_release(execution_id).await;
    }

    /// Publish the more specific "stopped without a PR" signal so the
    /// frontend can paint a distinct activity icon (the live-state
    /// chore picks this up). Falls back to the same status string as
    /// `awaiting_input` because the execution row hasn't moved.
    async fn publish_awaiting_pr(&self, execution: &crate::work::WorkExecution) {
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "worker_awaiting_pr",
            )
            .await;
    }

    /// Resolve the PR already bound to this execution's chore, if any.
    /// Mirrors [`Self::evaluate_sha_delta_gate`]'s resolution: the
    /// chore's own structured `pr_url` is authoritative (it is set by
    /// whichever sibling execution opened the PR — for a
    /// `ci_remediation` exec that is the `chore_implementation` exec
    /// that shipped the original change). `revision_implementation`
    /// chores carry `pr_url = NULL` by design, so for that kind we fall
    /// back to `execution.pr_url` (the chain root's PR, stamped at
    /// dispatch).
    ///
    /// Used by the nudge path to decide whether a "produce a PR" nudge
    /// is even appropriate: when a PR is already bound the worker must
    /// be pointed at the existing branch, never told to `gh pr create`.
    fn resolve_bound_pr_url(&self, execution: &crate::work::WorkExecution) -> Option<String> {
        match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => {
                crate::runner::task_bound_pr_url(&task)
                    .map(str::to_owned)
                    .or_else(|| {
                        if execution.kind == ExecutionKind::RevisionImplementation {
                            // Primary: execution.pr_url is stamped at dispatch time.
                            // Fallback: walk the parent chain to find the chain root's
                            // pr_url for executions where execution.pr_url was not set
                            // (e.g. older executions predating reliable dispatch stamping).
                            execution
                                .pr_url
                                .clone()
                                .filter(|u| !u.is_empty())
                                .or_else(|| {
                                    self.work_db
                                        .get_revision_chain_root_pr_url(&task.id)
                                })
                        } else {
                            None
                        }
                    })
            }
            _ => None,
        }
    }

    /// Generic auto-nudge gate. Records the intent to nudge `execution`
    /// against the circuit breaker (keyed by `fingerprint`, which must
    /// encode the work state so an unchanged state counts as
    /// unproductive) and either:
    ///
    /// - queues `probe_text`, publishes the awaiting-PR signal, and
    ///   returns `proceed_outcome` (the nudge fired); or
    /// - parks the execution via [`Self::park_for_unproductive_nudges`]
    ///   and returns [`StopOutcome::NudgeBreakerParked`] (the breaker
    ///   tripped — `max_unproductive_nudges` consecutive nudges fired
    ///   with no state change).
    ///
    /// This is the single choke point for the nudge loop: bounding it
    /// here makes the breaker generic to *every* auto-nudge, not just
    /// the "produce a PR" one.
    async fn nudge_or_park(
        &self,
        execution: &crate::work::WorkExecution,
        probe_text: &str,
        fingerprint: &str,
        bound_pr_url: Option<&str>,
        proceed_outcome: StopOutcome,
    ) -> StopOutcome {
        match self.nudge_breaker.record(
            &execution.id,
            fingerprint,
            self.max_unproductive_nudges,
        ) {
            NudgeDecision::Proceed { count } => {
                tracing::info!(
                    execution_id = %execution.id,
                    nudge_count = count,
                    max = self.max_unproductive_nudges,
                    "auto-nudge: queueing probe (under circuit-breaker cap)"
                );
                self.publish_awaiting_pr(execution).await;
                self.probe_queuer.queue_probe(&execution.id, probe_text);
                proceed_outcome
            }
            NudgeDecision::Trip { count } => {
                self.park_for_unproductive_nudges(
                    execution,
                    count,
                    bound_pr_url,
                    "no new commit, PR, or state change",
                )
                .await
            }
        }
    }

    /// Park `execution` because the auto-nudge circuit breaker tripped
    /// (or because nudging it is structurally wrong, e.g. a
    /// `ci_remediation` exec with no bound PR). Files a (deduplicated)
    /// attention item with a human-readable reason and publishes
    /// `AttentionItemCreated` so the coordinator/UI surfaces it, then
    /// publishes a distinct live-state reason. The execution stays in
    /// `waiting_human` — that *is* the parked-for-human state — but the
    /// engine stops nudging it.
    async fn park_for_unproductive_nudges(
        &self,
        execution: &crate::work::WorkExecution,
        nudge_count: u32,
        bound_pr_url: Option<&str>,
        detail: &str,
    ) -> StopOutcome {
        let pr_clause = match bound_pr_url {
            Some(url) => format!("A PR already exists for this work: {url}."),
            None => "No PR was produced.".to_owned(),
        };
        let reason = if nudge_count > 0 {
            format!(
                "Auto-nudge circuit breaker tripped: nudged {nudge_count} times with {detail}. \
{pr_clause} Parked for human review."
            )
        } else {
            format!("Worker parked without nudging: {detail}. {pr_clause}")
        };

        // Deduplicate: only one open attention item of this kind per
        // execution, so repeated Stops after the breaker trips don't
        // pile up identical items.
        let already_filed = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items.iter().any(|i| {
                    i.kind == NUDGE_BREAKER_ATTENTION_KIND && i.status != "resolved"
                })
            })
            .unwrap_or(false);
        if !already_filed {
            match self.work_db.create_attention_item(CreateAttentionItemInput {
                execution_id: Some(execution.id.clone()),
                work_item_id: None,
                kind: NUDGE_BREAKER_ATTENTION_KIND.to_owned(),
                status: None,
                title: "Worker parked: auto-nudge loop bounded".to_owned(),
                body_markdown: reason.clone(),
                resolved_at: None,
            }) {
                Ok(item) => {
                    if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                        let product_id = work_item_product_id(&work_item);
                        self.publisher
                            .publish_frontend_event_on_product(
                                &product_id,
                                FrontendEvent::AttentionItemCreated { item },
                            )
                            .await;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "nudge breaker: failed to file attention item; parking without UI surface"
                    );
                }
            }
        }

        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "worker_nudge_breaker_parked",
            )
            .await;
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            kind = %execution.kind,
            nudge_count,
            %reason,
            "auto-nudge circuit breaker tripped — parked execution, no further nudges"
        );
        StopOutcome::NudgeBreakerParked { reason }
    }

    /// Hook invoked once when an execution transitions to `running`
    /// (from the coordinator's `start_execution_run` path). If the
    /// bound chore already has a PR URL (i.e. this is a resume /
    /// bounce-back of an already-bound chore), capture the PR's
    /// current head SHA into `work_executions.pr_head_before` so the
    /// Stop-boundary SHA-delta gate can verify the run's contribution.
    ///
    /// Best-effort: every step is fallible (work item lookup, repo
    /// slug parsing, PR number parsing, GitHub fetch) and on failure
    /// we log at WARN and leave `pr_head_before` unset. The gate
    /// treats a missing snapshot as "inapplicable" and falls through
    /// to the existing branch-keyed detector path — never noisier
    /// than the pre-change behaviour.
    pub async fn on_execution_started(&self, execution_id: &str) {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "execution_started hook: unknown execution — skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let bound_pr_url = match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => {
                // For revision_implementation executions the task's own
                // pr_url is always NULL (design: revision tasks don't own
                // a PR).  Fall back to execution.pr_url (stamped at
                // dispatch), then to a chain-root lookup for executions
                // where execution.pr_url was not reliably set.
                crate::runner::task_bound_pr_url(&task)
                    .map(str::to_owned)
                    .or_else(|| {
                        if execution.kind == ExecutionKind::RevisionImplementation {
                            execution
                                .pr_url
                                .clone()
                                .filter(|u| !u.is_empty())
                                .or_else(|| {
                                    self.work_db
                                        .get_revision_chain_root_pr_url(&task.id)
                                })
                        } else {
                            None
                        }
                    })
            }
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "execution_started hook: work item lookup failed; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let bound_pr_url = match bound_pr_url {
            Some(url) => url,
            None => {
                tracing::debug!(
                    execution_id,
                    "execution_started hook: chore has no bound pr_url — new-PR flow, no snapshot needed"
                );
                return;
            }
        };
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    repo_remote_url = %execution.repo_remote_url,
                    ?err,
                    "execution_started hook: cannot parse repo slug; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let pr_number = match pr_number_from_url(&bound_pr_url) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "execution_started hook: cannot parse PR number from bound URL; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let head_oid = match self
            .branch_verifier
            .fetch_pr_head_oid(&repo_slug, pr_number)
            .await
        {
            Ok(oid) => oid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "execution_started hook: fetch headRefOid failed; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        if let Err(err) = self
            .work_db
            .set_execution_pr_head_before(execution_id, &head_oid)
        {
            tracing::warn!(
                execution_id,
                ?err,
                "execution_started hook: failed to persist pr_head_before"
            );
            return;
        }
        tracing::info!(
            execution_id,
            bound_pr_url = %bound_pr_url,
            head_oid = %head_oid,
            "execution_started hook: snapshotted pr_head_before for SHA-delta gate"
        );

        // Also snapshot the PR body as the baseline for the metadata-only
        // CI-fix finalize gate (issue #1252). A CI-fix revision that
        // repairs a PR-description validator edits the body with no commit,
        // so the head SHA never moves; the body diff against this snapshot
        // is the only operator-visible evidence the worker contributed.
        // Best-effort and independent of downstream finalisation — an empty
        // body is a valid snapshot, but a fetch failure leaves it unset and
        // the gate treats that as inapplicable.
        match self
            .branch_verifier
            .fetch_pr_body(&repo_slug, pr_number)
            .await
        {
            Ok(body) => {
                if let Err(err) = self.work_db.set_execution_pr_body_before(execution_id, &body) {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "execution_started hook: failed to persist pr_body_before"
                    );
                } else {
                    tracing::debug!(
                        execution_id,
                        bound_pr_url = %bound_pr_url,
                        body_len = body.len(),
                        "execution_started hook: snapshotted pr_body_before for metadata-fix gate"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "execution_started hook: fetch PR body failed; skipping pr_body_before snapshot"
                );
            }
        }
    }

    /// Check whether the blocking signal for a conflict-resolution or
    /// CI-failure revision is already cleared even though the worker
    /// did not push any new commits (the `NoContribution` SHA-delta
    /// outcome). Returns `Some(outcome)` to short-circuit the nudge
    /// path on success; `None` falls through to the normal nudge.
    ///
    /// Probe result: if the merge probe is unavailable (e.g.
    /// [`NoopMergeProbe`] in tests, transient `gh` failure), the
    /// method returns `None` so the nudge fires as before — safe
    /// fallback.
    ///
    /// Anti-re-entrancy: the attempt is marked `succeeded` before
    /// `finalize_pr_transition` runs, so a concurrent sweep cannot
    /// dispatch a new attempt for the same parent-chore signal.
    async fn try_retire_cleared_blocking_signal(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        // Determine the parent chore ID that owns the conflict_resolutions /
        // ci_remediations attempt rows.
        //
        // Old-style kinds: `execution.work_item_id` IS the parent chore.
        // New-style `revision_implementation` (Phase 3+): `work_item_id` is
        // the revision task; the chore is its `parent_task_id`.
        let (parent_chore_id, product_id) = match execution.kind {
            ExecutionKind::ConflictResolution | ExecutionKind::CiRemediation => {
                // work_item_id is the chore directly.
                let product_id = match self.work_db.get_work_item(&execution.work_item_id) {
                    Ok(WorkItem::Task(t) | WorkItem::Chore(t)) => t.product_id.clone(),
                    _ => return None,
                };
                (execution.work_item_id.clone(), product_id)
            }
            ExecutionKind::RevisionImplementation => {
                // work_item_id is the revision task. Only process it when
                // the revision was created by an engine-triggered conflict /
                // CI-fix attempt (`created_via` prefix).
                let task = match self.work_db.get_work_item(&execution.work_item_id) {
                    Ok(WorkItem::Task(t)) if t.kind == TaskKind::Revision => t,
                    _ => return None,
                };
                let created_via = task.created_via.as_str();
                if !created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
                    && !created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX)
                {
                    return None;
                }
                let parent_id = task.parent_task_id?;
                (parent_id, task.product_id.clone())
            }
            _ => return None,
        };

        // Check for an active conflict-resolution or CI-remediation attempt.
        let conflict_attempt = self
            .work_db
            .active_conflict_resolution_for_work_item(&parent_chore_id)
            .unwrap_or(None);
        let ci_attempt = self
            .work_db
            .active_ci_remediation_for_work_item(&parent_chore_id)
            .unwrap_or(None);

        if conflict_attempt.is_none() && ci_attempt.is_none() {
            return None;
        }

        // Probe the bound PR to check if the blocking signal is cleared.
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "stop event: signal-cleared check: PR probe failed; \
                     falling through to nudge",
                );
                return None;
            }
        };
        let open_status = match probe.state {
            PrLifecycleState::Open(ref s) => s.clone(),
            // PR merged or closed — not a signal-cleared case here.
            _ => return None,
        };

        // --- Conflict signal check ---
        if let Some(ref attempt) = conflict_attempt
            && open_status.mergeability != OpenPrMergeability::Conflict {
                tracing::info!(
                    execution_id,
                    attempt_id = %attempt.id,
                    bound_pr_url,
                    "stop event: conflict already cleared — retiring attempt without nudging"
                );
                // Mark the attempt succeeded.
                match self
                    .work_db
                    .mark_conflict_resolution_succeeded(&attempt.id, None)
                {
                    Ok(Some(succeeded)) => {
                        // Release old-style cube lease on the attempt (null for
                        // new Phase 3 revision-backed attempts).
                        if let Some(lease_id) = succeeded.cube_lease_id.as_deref()
                            && let Err(err) =
                                self.cube_client.release_workspace(lease_id).await
                            {
                                tracing::debug!(
                                    attempt_id = %attempt.id,
                                    lease_id,
                                    ?err,
                                    "signal-cleared: conflict lease release failed \
                                     (likely already released)",
                                );
                            }
                        self.publisher
                            .publish_frontend_event_on_product(
                                &product_id,
                                FrontendEvent::ConflictResolutionSucceeded {
                                    product_id: product_id.clone(),
                                    work_item_id: parent_chore_id.clone(),
                                    attempt_id: succeeded.id.clone(),
                                    pr_url: bound_pr_url.to_owned(),
                                },
                            )
                            .await;
                    }
                    Ok(None) => {
                        tracing::debug!(
                            attempt_id = %attempt.id,
                            "signal-cleared: conflict attempt already terminal"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            ?err,
                            "signal-cleared: failed to mark conflict_resolution succeeded"
                        );
                    }
                }
                // Snap parent chore back to in_review.
                match self.work_db.clear_chore_blocked_merge_conflict_for_attempt(
                    &parent_chore_id,
                    bound_pr_url,
                    &attempt.id,
                ) {
                    Ok(Some(_)) => {
                        self.publisher
                            .publish_work_item_changed(
                                &product_id,
                                &parent_chore_id,
                                "merge_conflict_resolved",
                            )
                            .await;
                    }
                    Ok(None) => {
                        // WHERE guard missed — parent was already moved (e.g.
                        // by a concurrent sweep or human). Fine.
                    }
                    Err(err) => {
                        tracing::warn!(
                            work_item_id = %parent_chore_id,
                            ?err,
                            "signal-cleared: failed to clear blocked: merge_conflict"
                        );
                    }
                }
                let outcome = self
                    .finalize_pr_transition(
                        execution_id,
                        bound_pr_url.to_owned(),
                        WorkerPrCompletionTarget::InReview,
                        "stop_conflict_cleared",
                    )
                    .await;
                // Return the distinct outcome so tests and logs can identify this path.
                return Some(match outcome {
                    StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
                        StopOutcome::SignalAlreadyCleared { pr_url }
                    }
                    other => other,
                });
            }

        // --- CI signal check ---
        if let Some(ref attempt) = ci_attempt
            && ci_attempt_signal_cleared(&attempt.failed_checks, &open_status.ci) {
                tracing::info!(
                    execution_id,
                    attempt_id = %attempt.id,
                    bound_pr_url,
                    "stop event: CI already cleared — retiring attempt without nudging"
                );
                match self
                    .work_db
                    .mark_ci_remediation_succeeded(&attempt.id, None)
                {
                    Ok(Some(_)) => {
                        self.publisher
                            .publish_frontend_event_on_product(
                                &product_id,
                                FrontendEvent::CiRemediationSucceeded {
                                    product_id: product_id.clone(),
                                    work_item_id: parent_chore_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: bound_pr_url.to_owned(),
                                },
                            )
                            .await;
                    }
                    Ok(None) => {
                        tracing::debug!(
                            attempt_id = %attempt.id,
                            "signal-cleared: CI attempt already terminal"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            ?err,
                            "signal-cleared: failed to mark ci_remediation succeeded"
                        );
                    }
                }
                match self
                    .work_db
                    .clear_chore_blocked_ci_failure(&parent_chore_id, bound_pr_url)
                {
                    Ok(Some(_)) => {
                        self.publisher
                            .publish_work_item_changed(
                                &product_id,
                                &parent_chore_id,
                                "ci_failure_resolved",
                            )
                            .await;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(
                            work_item_id = %parent_chore_id,
                            ?err,
                            "signal-cleared: failed to clear blocked: ci_failure"
                        );
                    }
                }
                let outcome = self
                    .finalize_pr_transition(
                        execution_id,
                        bound_pr_url.to_owned(),
                        WorkerPrCompletionTarget::InReview,
                        "stop_ci_cleared",
                    )
                    .await;
                return Some(match outcome {
                    StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
                        StopOutcome::SignalAlreadyCleared { pr_url }
                    }
                    other => other,
                });
            }

        None
    }

    /// On-Stop arm of the metadata-only CI-fix finalize gate (issue
    /// #1252). Called from `on_stop_inner`'s `NoContribution` branch —
    /// i.e. at a *real* Stop boundary where the bound PR head did not move
    /// this run.
    ///
    /// Detects whether this revision produced an operator-visible
    /// PR-metadata delta (the live PR body differs from the
    /// `pr_body_before` snapshot taken at run start). If so it:
    ///   - stamps the `metadata_fix_confirmed_at` marker — positive
    ///     evidence (real Stop boundary + operator-visible delta) that the
    ///     merge poller consumes when CI greens *after* this Stop, and
    ///   - finalizes immediately if CI is already green (returning the
    ///     finalize outcome); otherwise returns `AwaitingInput` (recorded,
    ///     awaiting CI — deliberately NOT a nudge, because a metadata-only
    ///     fix has nothing to push to the existing PR).
    ///
    /// Returns `None` when this is not a metadata-only fix (not a
    /// revision, no baseline snapshot, fetch failure, or the body is
    /// unchanged) so the caller falls through to its normal nudge: head
    /// unchanged AND body unchanged means the worker contributed nothing
    /// this run, which must NOT be mistaken for a clean no-op completion.
    async fn try_finalize_metadata_only_fix_on_stop(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        if execution.kind != ExecutionKind::RevisionImplementation {
            return None;
        }
        // Baseline body snapshot from run start. `None` means no baseline
        // (new-PR flow, or the start-of-run fetch failed) — we cannot prove
        // a delta, so fall through to the normal nudge.
        let before = match self.work_db.get_execution_pr_body_before(execution_id) {
            Ok(Some(body)) => body,
            Ok(None) => return None,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "metadata-fix on-stop: pr_body_before read failed; falling through to nudge",
                );
                return None;
            }
        };
        let repo_slug = parse_repo_slug(&execution.repo_remote_url).ok()?;
        let pr_number = pr_number_from_url(bound_pr_url)?;
        let current = match self
            .branch_verifier
            .fetch_pr_body(&repo_slug, pr_number)
            .await
        {
            Ok(body) => body,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "metadata-fix on-stop: live PR body fetch failed; falling through to nudge",
                );
                return None;
            }
        };
        if current == before {
            // No operator-visible delta: head unchanged AND body unchanged.
            // The worker contributed nothing this run — let the caller nudge.
            return None;
        }
        // Operator-visible PR-metadata delta produced at a real Stop
        // boundary. Persist the positive-evidence marker BEFORE attempting
        // to finalize so a transient probe failure still lets the merge
        // poller finalize once CI goes green.
        if let Err(err) = self
            .work_db
            .mark_execution_metadata_fix_confirmed(execution_id)
        {
            tracing::warn!(
                execution_id,
                ?err,
                "metadata-fix on-stop: failed to persist confirmation marker",
            );
        }
        if let Some(outcome) = self
            .finalize_metadata_only_revision_if_ready(execution_id, bound_pr_url)
            .await
        {
            return Some(outcome);
        }
        // Delta recorded but CI not yet green: return quietly (no nudge —
        // there is nothing to push). The merge poller's `recheck_for_pr`
        // finalizes once CI goes green, gated on the marker just stamped.
        tracing::info!(
            execution_id,
            bound_pr_url,
            "stop event: PR-metadata-only CI fix recorded; awaiting CI to go green before \
             finalizing (issue #1252)",
        );
        Some(StopOutcome::AwaitingInput)
    }

    /// Finalize a metadata-only CI-fix revision IF its bound PR is now in
    /// a demonstrably-healthy state. Probes the bound parent PR and
    /// decides from its live state:
    ///   - open with clean CI → the fix landed → finalize to `in_review`;
    ///   - already merged      → finalize to `done`;
    ///   - CI still failing / in-flight, closed-unmerged, or the probe
    ///     failed → return `None` (caller leaves it for a later sweep).
    ///
    /// Callers MUST first establish the positive evidence that this is a
    /// legitimate no-code-change completion: a *real* Stop boundary that
    /// observed an operator-visible PR-metadata delta. `on_stop` is itself
    /// that boundary; the merge poller gates on the
    /// `metadata_fix_confirmed_at` marker `on_stop` stamps. This helper
    /// only re-checks CI — it deliberately does NOT re-derive the
    /// Stop/delta evidence, so the regression-prone "head unchanged + CI
    /// green" inference (#1262) can never be reached without it.
    /// Idempotent against an already-finalized execution
    /// (`finalize_pr_transition` returns `AlreadyTerminal` for a non-live
    /// row).
    async fn finalize_metadata_only_revision_if_ready(
        &self,
        execution_id: &str,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "metadata-fix finalize: bound-PR probe failed; will retry on a later sweep",
                );
                return None;
            }
        };
        let target = match &probe.state {
            // Require BOTH clean CI and clean mergeability: a metadata-only
            // edit must not finalize a PR that still carries a *separate*
            // blocking signal (e.g. a merge conflict on a conflict-resolution
            // revision the worker did not actually rebase). Only a genuinely
            // review-ready PR advances.
            PrLifecycleState::Open(open)
                if open.mergeability == OpenPrMergeability::Clean
                    && matches!(open.ci, OpenPrCiStatus::Clean) =>
            {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "metadata-fix finalize: bound PR open, mergeable, with clean CI and a \
                     Stop-confirmed PR-metadata delta — finalizing the revision to in_review \
                     (issue #1252)",
                );
                WorkerPrCompletionTarget::InReview
            }
            PrLifecycleState::Merged => {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "metadata-fix finalize: bound PR already merged — finalizing the revision \
                     to done (issue #1252)",
                );
                WorkerPrCompletionTarget::Done
            }
            // CI still failing / in-flight, or PR closed-unmerged: the fix
            // has not demonstrably landed. Leave it; a later sweep re-probes.
            _ => return None,
        };
        Some(
            self.finalize_pr_transition(
                execution_id,
                bound_pr_url.to_owned(),
                target,
                "metadata_only_fix",
            )
            .await,
        )
    }

    /// Evaluate the resume-bounce SHA-delta gate. The gate uses the
    /// chore's bound `pr_url` (set by an earlier run's on-Stop
    /// machinery) as the authoritative PR identifier — never
    /// branch-search — and verifies "this run contributed" by
    /// comparing the bound PR's current head SHA against the
    /// snapshot in `execution.pr_head_before`. See [`Self::on_execution_started`]
    /// for the snapshot path.
    async fn evaluate_sha_delta_gate(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
    ) -> ShaDeltaGateOutcome {
        // The chore-bound PR URL is the only authoritative identifier
        // permitted here. No branch search.
        let work_item = match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(item) => item,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "sha-delta gate: work item lookup failed; treating as inapplicable"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let bound_pr_url = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                // Primary: the task's own pr_url (structured field only).
                // Fallback for revision_implementation: use execution.pr_url
                // (set to the chain root's PR URL at dispatch time), because
                // revision tasks always have task.pr_url = NULL by design.
                let from_task = crate::runner::task_bound_pr_url(&task).map(str::to_owned);
                match from_task {
                    Some(url) => url,
                    None if execution.kind == ExecutionKind::RevisionImplementation => {
                        // Primary: execution.pr_url stamped at dispatch time.
                        // Fallback: chain-root lookup for executions where it
                        // was not stamped.
                        match execution
                            .pr_url
                            .clone()
                            .filter(|u| !u.is_empty())
                            .or_else(|| {
                                self.work_db
                                    .get_revision_chain_root_pr_url(&task.id)
                            }) {
                            Some(url) => url,
                            None => return ShaDeltaGateOutcome::Inapplicable,
                        }
                    }
                    None => return ShaDeltaGateOutcome::Inapplicable,
                }
            }
            _ => return ShaDeltaGateOutcome::Inapplicable,
        };
        let pr_head_before = match execution.pr_head_before.as_deref() {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "sha-delta gate: bound PR present but pr_head_before snapshot missing; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    repo_remote_url = %execution.repo_remote_url,
                    ?err,
                    "sha-delta gate: cannot parse repo slug; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let pr_number = match pr_number_from_url(&bound_pr_url) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "sha-delta gate: cannot parse PR number from bound URL; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let head_now = match self
            .branch_verifier
            .fetch_pr_head_oid(&repo_slug, pr_number)
            .await
        {
            Ok(oid) => oid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "sha-delta gate: fetch headRefOid failed; falling through to cold-path detector"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        if head_now == pr_head_before {
            tracing::info!(
                execution_id,
                bound_pr_url = %bound_pr_url,
                pr_head_before = %pr_head_before,
                "sha-delta gate: bound PR head unchanged — worker did not contribute"
            );
            ShaDeltaGateOutcome::NoContribution {
                pr_url: bound_pr_url,
            }
        } else {
            tracing::info!(
                execution_id,
                bound_pr_url = %bound_pr_url,
                pr_head_before = %pr_head_before,
                head_now = %head_now,
                "sha-delta gate: bound PR head moved — contribution verified"
            );
            ShaDeltaGateOutcome::Contributed { pr_url: bound_pr_url }
        }
    }
}

#[async_trait]
impl crate::coordinator::ExecutionStartedHook for WorkerCompletionHandler {
    async fn on_execution_started(&self, execution_id: &str) {
        // Inherent method already does the work; this just satisfies
        // the trait the coordinator depends on.
        WorkerCompletionHandler::on_execution_started(self, execution_id).await
    }
}

/// Outcome of the resume-bounce SHA-delta gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShaDeltaGateOutcome {
    /// The chore had a bound `pr_url`, snapshot+current SHA were
    /// fetched, and the SHAs differ — this run moved the bound PR.
    /// Caller should finalize via `finalize_pr_transition(InReview)`.
    Contributed { pr_url: String },
    /// The chore had a bound `pr_url`, snapshot+current SHA were
    /// fetched, and the SHAs are equal — this run did not move the
    /// bound PR. Caller nudges the worker to push to the *existing*
    /// bound PR (never `gh pr create`), bounded by the circuit breaker,
    /// without falling through to the cold-path branch detector.
    NoContribution { pr_url: String },
    /// The gate could not evaluate (no bound PR, no snapshot, or a
    /// fetch failure). Caller falls through to the existing
    /// branch-keyed cold-path detector — preserves pre-change
    /// behaviour for the new-PR flow.
    Inapplicable,
}

/// Attention-item `kind` filed when the auto-nudge circuit breaker
/// parks an execution. Distinct kind so the coordinator/UI can surface
/// "worker parked: nudge loop bounded" separately from other attention
/// flows, and so repeated Stops dedupe against an already-open item.
pub const NUDGE_BREAKER_ATTENTION_KIND: &str = "nudge_breaker_tripped";

/// Probe text dispatched when a worker stops without producing any PR
/// for its branch. Phrased so a worker that already finished the work
/// will simply push and open one, but a worker that's blocked has an
/// out to explain itself rather than churning.
pub const PROBE_NO_PR: &str = "You stopped without producing a PR for this work. \
If the work is complete, push your branch and open the PR with `gh pr create`. \
If you're blocked, explain what you need.";

/// Extract the set of required-check names a `ci_remediations` attempt
/// was opened to fix, parsed from its `failed_checks` JSON snapshot
/// (each entry carries a `"name"` field; see `ci_watch::FailedCheckRecord`).
/// An empty array, malformed JSON, or entries without a name yield an
/// empty list — callers treat that as "no targeted-check information"
/// and fall back to requiring whole-PR `Clean`.
fn targeted_check_names(failed_checks_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(failed_checks_json)
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Array(arr) => Some(arr),
            _ => None,
        })
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the CI blocking signal *this attempt was opened for* is now
/// cleared on the bound PR.
///
/// The original heuristic keyed solely on whole-PR `Clean` (every
/// required check terminal-green). That misses a legitimate completion:
/// a remediation opened for one specific failing check (e.g. the "Pull
/// Request Description" check, often fixed by a metadata-only
/// `gh pr edit` with no commit) whose own check has gone green while
/// *other, unrelated* required checks remain red or pending. Such an
/// attempt has done its job and must be retired — not nudged forever to
/// "push your commits".
///
/// Decision table:
/// - `Clean`   → cleared (all required green; trivially clears any attempt).
/// - `Failing` → cleared iff none of the attempt's targeted checks are
///   among the currently-failing set. Remaining failures belong to other
///   checks and will drive their own remediation once the parent snaps
///   back to `in_review`.
/// - `InFlight`→ never cleared: at least one required check is still
///   non-terminal and we cannot tell from this aggregate whether the
///   targeted check specifically has reached terminal-green yet. Stay
///   conservative; the next sweep re-evaluates once checks terminalize.
///
/// When the attempt carries no parseable targeted-check names, only the
/// `Clean` case clears it — preserving the pre-change behaviour.
fn ci_attempt_signal_cleared(attempt_failed_checks: &str, ci: &OpenPrCiStatus) -> bool {
    match ci {
        OpenPrCiStatus::Clean => true,
        OpenPrCiStatus::InFlight => false,
        OpenPrCiStatus::Failing { failures } => {
            let targeted = targeted_check_names(attempt_failed_checks);
            if targeted.is_empty() {
                return false;
            }
            let failing_names: std::collections::HashSet<&str> =
                failures.iter().map(|f| f.name.as_str()).collect();
            !targeted
                .iter()
                .any(|name| failing_names.contains(name.as_str()))
        }
    }
}

/// Probe text dispatched when a PR is already bound to the worker's
/// chore (a resume, or a `ci_remediation` exec whose sibling
/// `chore_implementation` opened the PR). The worker must NEVER be told
/// to `gh pr create` here — its job is to push fixes to the existing
/// PR's branch. Phrased so a worker with nothing left to do can say so
/// rather than churning; the circuit breaker bounds repeats.
pub fn probe_push_to_existing_pr(pr_url: &str) -> String {
    format!(
        "A PR already exists for this work: {pr_url}. Do NOT open a new PR. If you have local \
commits, push them to the existing PR's branch (`jj git push -b <bookmark>`). If your changes \
are already pushed or there is nothing left to do, say so — explain your status instead of \
re-running."
    )
}

/// Probe text dispatched when a PR exists but the worker has local
/// commits that haven't been pushed yet — the PR is stale.
pub const PROBE_STALE_PR: &str = "A PR exists for this branch, but your local commits \
are ahead of the PR's head. Push the new commits (`jj git push -b <bookmark>`) \
so the PR reflects your latest work, or explain why the local commits should not \
be pushed.";

/// Probe text dispatched when a PR exists and head_match is satisfied,
/// but the PR contains no file changes (`changed_files == 0`). The
/// worker likely pushed an empty commit without making any edits.
pub const PROBE_EMPTY_PR: &str = "The PR you opened has an empty diff — no files were \
changed. This usually means you committed and pushed without making any edits. \
Run `jj diff -r @` to verify your working-copy changes. If the diff is empty, \
you have not made any changes — do not keep this PR open. Either make the required \
edits and push them, or close the PR and explain what went wrong.";

/// What happened during a stop event handler invocation. The runtime
/// only logs this; tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Stop arrived for a run id that doesn't map to a known execution
    /// (e.g., test infra, agent runs).
    UnknownExecution,
    /// Execution was already in a terminal status — no transition.
    AlreadyTerminal,
    /// The Stop arrived for an execution that is a stale prior occupant
    /// of a reused (warm-cached) cube workspace — a newer live execution
    /// now claims the same workspace. The event leaked from a stale
    /// `boss-event` hook registration left in the re-leased workspace;
    /// processing it would mis-attribute completion or release the live
    /// run's workspace. Quiet outcome — no transition, no reap. The
    /// newest execution's own Stop drives its completion.
    SupersededInWorkspace,
    /// Execution had no workspace_path recorded.
    NoWorkspace,
    /// `gh` failed with a non-"no-PR" error; surfaced as awaiting input.
    DetectorFailed,
    /// No PR yet — worker is idle awaiting input.
    AwaitingInput,
    /// AI #6 / incident 001: the Stop hook fired for an execution in
    /// `running` status with an empty staged-URL cache. The fallback
    /// is reserved for `waiting_human`; the worker is still alive and
    /// any positive result would race against its own in-flight push.
    /// Quiet outcome — no probe, no publish, no transition.
    RunningNoStagedPr,
    /// AI #5 / incident 001: the human has flipped the
    /// `detect_pr_cold_fallback` feature flag OFF via the debug pane,
    /// so the cold-path fallback is suppressed. With no staged URL
    /// the engine treats the empty staging as "no PR pushed" — the
    /// chore stays in `waiting_human` for the human to resolve by
    /// hand. Quiet outcome — no probe, no publish, no transition.
    FallbackDisabledByFlag,
    /// PR detected; work item moved to `in_review` and execution finalised.
    PrDetected { pr_url: String },
    /// PR detected and already merged at Stop time; work item moved
    /// straight to `done` and execution finalised.
    PrMerged { pr_url: String },
    /// PR exists but local commits are ahead of its head sha. The
    /// worker is probed to push the missing commits; the work item
    /// stays in its current state until the next Stop reports a fresh PR.
    ///
    /// Post-incident-001 the branch-keyed detector cannot produce this
    /// classification (there is no SHA matching to fail). The variant
    /// is kept for callers that already pattern-match on it.
    StalePr { pr_url: String, reason: String },
    /// PR exists and head_match, but has zero file changes. The worker
    /// is probed to make real edits or close the PR; the work item
    /// stays in its current state.
    EmptyDiffPr { pr_url: String },
    /// The auto-nudge circuit breaker tripped: the worker was nudged
    /// `max_unproductive_nudges` consecutive times with no new commit,
    /// PR, or state transition. The execution is parked (an attention
    /// item is filed and an `AttentionItemCreated` event published)
    /// instead of being nudged again. `reason` is the human-readable
    /// explanation recorded on the attention item.
    NudgeBreakerParked { reason: String },
    /// The worker is a conflict-resolution or CI-failure revision that
    /// stopped without pushing, but the blocking signal was already
    /// cleared (conflict: PR `mergeable`; CI: required checks green)
    /// before this run started. The active `conflict_resolutions` or
    /// `ci_remediations` attempt is retired as `succeeded`, the parent
    /// task is snapped back to `in_review`, and the execution is
    /// finalised. No nudge is sent.
    SignalAlreadyCleared { pr_url: String },
    /// A CI-remediation worker classified the failure as flaky/infra and
    /// re-triggered the failing job (`boss engine ci mark-retriggered`),
    /// which stamped the `ci_flaky_retriggered` signal on the parent.
    /// There is genuinely nothing to push, so the completion path parks
    /// the worker — awaiting the CI retry or a human decision — instead of
    /// probing it for a diff. No nudge is sent; the execution is left
    /// `waiting_human`. This is the fix for the stuck-loop bug where the
    /// engine re-derived the same flaky verdict on every probe.
    FlakyRetriggered { pr_url: String },
    /// Maint task 6: an `automation_triage` execution finished and its
    /// final message was run through the marker-protocol outcome detector.
    /// `outcome` is the `automation_runs.outcome` discriminator recorded
    /// (`produced_task` / `skipped` / `failed_will_retry`). The execution is
    /// finalised (`completed`) and its pane/workspace released regardless of
    /// which marker (if any) the agent emitted.
    AutomationTriage { outcome: String },
    /// P992 task 7: a primary-implementation worker's PR was detected and
    /// an independent reviewer pass has been enqueued. The producing task
    /// remains in `active` (Doing column) until the reviewer resolves.
    ReviewerEnqueued { pr_url: String },
    /// P992 task 7: a `pr_review` reviewer execution finished and the
    /// producing task has been advanced to `in_review`.
    ReviewPassCompleted { pr_url: String },
    /// P992 task 8: a `pr_review` reviewer execution found qualifying findings
    /// (at least one `critical`/`high` severity or `regression` category) and
    /// created a revision task on the producing task. The producing task is
    /// advanced to `in_review`; the revision is dispatched on the general
    /// worker pool to apply the feedback. Nothing is posted to GitHub.
    ReviewPassRevisionCreated { pr_url: String, revision_task_id: String },
    /// Unexpected DB failure while recording completion.
    DbError,
}

/// Outcome of reading a finished triage execution's final assistant message
/// from its transcript (see [`WorkerCompletionHandler::read_final_triage_message`]).
///
/// Distinguishing these states is what makes a `failed_will_retry` triage run
/// diagnosable from the run-history `detail`: "produced no transcript" (worker
/// session never started), "transcript unreadable", and "no assistant prose"
/// are very different failures from "the worker spoke but emitted no marker",
/// yet all four previously collapsed to the bare string
/// "triage ended without a decision marker".
#[derive(Debug, Clone, PartialEq, Eq)]
enum TriageTranscript {
    /// The final assistant text message — the one the marker parser scans.
    FinalMessage(String),
    /// No `transcript_path` was recorded for the execution. The worker session
    /// likely never started (or its run row was never linked to a transcript).
    NoPath,
    /// A transcript path was recorded but the file could not be read (lookup
    /// error or filesystem read error).
    Unreadable,
    /// The transcript parsed but contained no assistant text event — the worker
    /// emitted only tool calls / thinking, or crashed before any prose.
    NoAssistantText,
}

impl TriageTranscript {
    /// The final assistant message text, or `None` for any state in which no
    /// message could be read. Lets callers that only need the text (e.g. the
    /// `pr_review` finaliser) ignore the failure-state distinction.
    fn into_message(self) -> Option<String> {
        match self {
            TriageTranscript::FinalMessage(text) => Some(text),
            TriageTranscript::NoPath
            | TriageTranscript::Unreadable
            | TriageTranscript::NoAssistantText => None,
        }
    }
}

/// Build the `failed_will_retry` detail for a triage run that yielded no
/// usable decision, from the transcript readback state.
///
/// The `FinalMessage` arm keeps the stable "triage ended without a decision
/// marker" prefix (so existing log greps / dashboards keep matching) and
/// appends a bounded, single-line tail of what the agent actually said — the
/// single most useful breadcrumb when debugging why a marker was missing.
fn triage_no_decision_detail(transcript: &TriageTranscript) -> String {
    match transcript {
        TriageTranscript::FinalMessage(text) => format!(
            "triage ended without a decision marker; final message tail: {}",
            tail_snippet(text, 200)
        ),
        TriageTranscript::NoPath => "triage produced no transcript (no transcript path \
             recorded; the worker session may have failed to start)"
            .to_owned(),
        TriageTranscript::Unreadable => {
            "triage transcript could not be read from disk".to_owned()
        }
        TriageTranscript::NoAssistantText => "triage transcript contained no assistant \
             message (worker emitted no prose before stopping)"
            .to_owned(),
    }
}

/// Collapse `text` to a single-line tail of at most `max_chars` characters for
/// embedding in a run-history `detail`. Whitespace runs (including newlines)
/// collapse to single spaces; when truncated, the result is prefixed with `…`
/// so it reads as a tail rather than a head.
fn tail_snippet(text: &str, max_chars: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "(empty)".to_owned();
    }
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= max_chars {
        one_line
    } else {
        let tail: String = chars[chars.len() - max_chars..].iter().collect();
        format!("…{tail}")
    }
}

fn work_item_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.id.clone(),
    }
}

fn work_item_product_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.product_id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.product_id.clone(),
    }
}

/// Whether completing a primary-implementation execution with a fresh PR
/// should trigger an independent reviewer pass (P992 design §1). When this
/// returns true, the producing task's column transition is held in
/// `PendingReview`/Doing until the reviewer finalises.
///
/// `RevisionImplementation` is handled separately: only revisions that were
/// created by the automated reviewer itself (`created_via` starts with
/// [`CREATED_VIA_PR_REVIEW_PREFIX`]) feed back into the review loop. CI-fix,
/// conflict-resolution, and human-initiated revisions do NOT re-trigger a
/// reviewer pass; they advance directly to human Review. That distinction is
/// applied in [`should_enqueue_reviewer_for_revision`].
fn should_enqueue_reviewer_for_primary(kind: &ExecutionKind) -> bool {
    matches!(
        kind,
        ExecutionKind::ChoreImplementation | ExecutionKind::TaskImplementation
    )
}

/// Whether a `RevisionImplementation` execution should re-trigger a reviewer
/// pass (P992 design §7). Returns `true` when the revision task's `created_via`
/// field carries the [`CREATED_VIA_PR_REVIEW_PREFIX`] prefix, meaning it was
/// spawned by the automated reviewer. Returns `false` for CI-fix, conflict-
/// resolution, and human-initiated revisions (those advance directly to
/// human Review without another automated pass).
fn should_enqueue_reviewer_for_revision(task_id: &str, work_db: &crate::work::WorkDb) -> bool {
    use boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX;
    match work_db.get_work_item(task_id) {
        Ok(crate::work::WorkItem::Task(t)) | Ok(crate::work::WorkItem::Chore(t)) => {
            t.created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus,
    };

    #[test]
    fn tail_snippet_collapses_whitespace_and_keeps_tail() {
        // Short text passes through, single-lined.
        assert_eq!(tail_snippet("hello world", 200), "hello world");
        assert_eq!(tail_snippet("a\n\nb   c", 200), "a b c");
        // Empty / whitespace-only → explicit marker, never a bare "".
        assert_eq!(tail_snippet("", 200), "(empty)");
        assert_eq!(tail_snippet("   \n  ", 200), "(empty)");
        // Over-length is truncated to the TAIL (the marker would be at the end
        // of a triage message) with a leading ellipsis.
        let long = "x".repeat(50);
        let snippet = tail_snippet(&long, 10);
        assert_eq!(snippet, format!("…{}", "x".repeat(10)));
        assert!(snippet.starts_with('…'));
    }

    #[test]
    fn triage_no_decision_detail_distinguishes_transcript_states() {
        // The "spoke but no marker" case keeps the stable prefix (so existing
        // greps match) and appends the agent's actual final words.
        let spoke = triage_no_decision_detail(&TriageTranscript::FinalMessage(
            "I looked around and decided to open a PR instead.".to_owned(),
        ));
        assert!(spoke.starts_with("triage ended without a decision marker"));
        assert!(spoke.contains("open a PR instead"));

        // The other states each get their own actionable phrasing — and must
        // NOT masquerade as "ended without a decision marker".
        let no_path = triage_no_decision_detail(&TriageTranscript::NoPath);
        assert!(no_path.contains("no transcript"));
        assert!(!no_path.contains("without a decision marker"));

        let unreadable = triage_no_decision_detail(&TriageTranscript::Unreadable);
        assert!(unreadable.contains("could not be read"));

        let no_prose = triage_no_decision_detail(&TriageTranscript::NoAssistantText);
        assert!(no_prose.contains("no assistant"));
    }

    #[test]
    fn triage_transcript_into_message_only_yields_final_message() {
        assert_eq!(
            TriageTranscript::FinalMessage("hi".to_owned()).into_message(),
            Some("hi".to_owned())
        );
        assert_eq!(TriageTranscript::NoPath.into_message(), None);
        assert_eq!(TriageTranscript::Unreadable.into_message(), None);
        assert_eq!(TriageTranscript::NoAssistantText.into_message(), None);
    }

    use crate::merge_poller::{MergeProbe, PrLifecycleProbe, PrLifecycleState};
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, FakePrStateChecker, PrOpenState,
        WorkDb, WorkItem,
    };

    /// Captured arguments from one `detect_pr` call. Tests assert on
    /// these to confirm the branch name passed in is execution-unique
    /// (the AI #6 regression guard: sibling workers in other cube
    /// workspaces must derive different branch names from their own
    /// execution IDs).
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DetectCall {
        repo_remote_url: String,
        expected_branch: String,
    }

    struct StubPrDetector {
        result: Mutex<Result<PrStatus, String>>,
        calls: std::sync::Mutex<Vec<DetectCall>>,
    }

    impl StubPrDetector {
        fn ok(value: Option<&str>) -> Arc<Self> {
            let status = match value {
                Some(url) => PrStatus::Fresh { url: url.to_owned() },
                None => PrStatus::None,
            };
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn ok_status(status: PrStatus) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Err(message.to_owned())),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// Swap the status returned by subsequent `detect_pr` calls.
        /// Lets a test model a worker that's idle for a couple of Stops
        /// and then finally opens a PR.
        async fn set_result(&self, status: PrStatus) {
            *self.result.lock().await = Ok(status);
        }

        fn call_count(&self) -> usize {
            self.calls
                .lock()
                .expect("StubPrDetector calls mutex poisoned")
                .len()
        }

        fn calls_snapshot(&self) -> Vec<DetectCall> {
            self.calls
                .lock()
                .expect("StubPrDetector calls mutex poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl PrDetector for StubPrDetector {
        async fn detect_pr(
            &self,
            repo_remote_url: &str,
            expected_branch: &str,
        ) -> Result<PrStatus> {
            self.calls
                .lock()
                .expect("StubPrDetector calls mutex poisoned")
                .push(DetectCall {
                    repo_remote_url: repo_remote_url.to_owned(),
                    expected_branch: expected_branch.to_owned(),
                });
            let guard = self.result.lock().await;
            match &*guard {
                Ok(value) => Ok(value.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }
    }

    /// Configurable branch verifier for tests. Returns a fixed
    /// `headRefName` (or error), a fixed `headRefOid` (or error), and a
    /// fixed diff line count (or error) without shelling out to `gh`.
    struct StubBranchVerifier {
        result: Result<String, String>,
        head_oid_result: Mutex<Result<String, String>>,
        /// Line count returned by `fetch_diff_line_count`. Defaults to
        /// `999` (non-trivial) so tests that don't exercise the skip gate
        /// never accidentally trigger a skip.
        diff_line_count_result: Mutex<Result<u64, String>>,
        body_result: Mutex<Result<String, String>>,
    }

    impl StubBranchVerifier {
        /// Verifier that always reports the given branch name. The
        /// `headRefOid` defaults to the literal string `"oid_unknown"`
        /// so tests that don't touch the SHA-delta path get a stable
        /// stand-in without having to wire one explicitly. Tests that
        /// exercise the gate call [`Self::with_head_oid`] to override.
        /// The PR body defaults to empty.
        fn ok(branch: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Ok(branch.to_owned()),
                head_oid_result: Mutex::new(Ok("oid_unknown".to_owned())),
                diff_line_count_result: Mutex::new(Ok(999)),
                body_result: Mutex::new(Ok(String::new())),
            })
        }

        /// Override the `headRefOid` returned by `fetch_pr_head_oid`.
        /// Used by the SHA-delta gate tests to simulate a PR whose
        /// head has (or has not) moved during the worker's run.
        async fn set_head_oid(&self, oid: Result<String, String>) {
            *self.head_oid_result.lock().await = oid;
        }

        /// Override the diff line count returned by `fetch_diff_line_count`.
        /// Tests that exercise the no-op / trivial-diff skip gate use this to
        /// simulate a pure rebase (0 lines) or trivially small change.
        async fn set_diff_line_count(&self, count: Result<u64, String>) {
            *self.diff_line_count_result.lock().await = count;
        }

        /// Override the body returned by `fetch_pr_body`. Used by the
        /// metadata-only CI-fix gate tests to simulate the live PR body
        /// the worker did (or did not) edit during its run.
        async fn set_body(&self, body: Result<String, String>) {
            *self.body_result.lock().await = body;
        }
    }

    #[async_trait]
    impl BranchVerifier for StubBranchVerifier {
        async fn fetch_pr_head_ref(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
            match &self.result {
                Ok(branch) => Ok(branch.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }

        async fn fetch_pr_head_oid(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
            let guard = self.head_oid_result.lock().await;
            match &*guard {
                Ok(oid) => Ok(oid.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }

        async fn fetch_diff_line_count(
            &self,
            _repo_slug: &str,
            _base: &str,
            _head: &str,
        ) -> Result<u64> {
            let guard = self.diff_line_count_result.lock().await;
            match &*guard {
                Ok(count) => Ok(*count),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }

        async fn fetch_pr_body(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
            let guard = self.body_result.lock().await;
            match &*guard {
                Ok(body) => Ok(body.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }
    }

    #[derive(Default)]
    struct RecordingProbeQueuer {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl ProbeQueuer for RecordingProbeQueuer {
        fn queue_probe(&self, run_id: &str, text: &str) {
            self.calls
                .lock()
                .expect("RecordingProbeQueuer mutex poisoned")
                .push((run_id.to_owned(), text.to_owned()));
        }
    }

    impl RecordingProbeQueuer {
        fn snapshot(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .expect("RecordingProbeQueuer mutex poisoned")
                .clone()
        }
    }

    #[derive(Default)]
    struct StubCubeClient {
        release_calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeClient for StubCubeClient {
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unreachable!("not used in completion tests")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
        ) -> Result<CubeWorkspaceLease> {
            unreachable!("not used in completion tests")
        }
        async fn create_change(
            &self,
            _: &Path,
            _: &str,
        ) -> Result<CubeChangeHandle> {
            unreachable!("not used in completion tests")
        }
        async fn release_workspace(&self, lease_id: &str) -> Result<()> {
            self.release_calls.lock().await.push(lease_id.to_owned());
            Ok(())
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unreachable!("not used in completion tests")
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

    #[derive(Default)]
    struct RecordingPaneReleaser {
        calls: Mutex<Vec<String>>,
        /// When set, `release_pane` reports this instead of the default
        /// `Reaped` — lets a test simulate a worker still mid-spawn
        /// (no slot mapped → `NoLiveWorker`) so the lease-release gate
        /// can be exercised.
        outcome: std::sync::Mutex<Option<PaneReleaseOutcome>>,
    }

    impl RecordingPaneReleaser {
        fn with_outcome(outcome: PaneReleaseOutcome) -> Self {
            Self {
                calls: Mutex::default(),
                outcome: std::sync::Mutex::new(Some(outcome)),
            }
        }
    }

    #[async_trait]
    impl WorkerPaneReleaser for RecordingPaneReleaser {
        async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
            self.calls.lock().await.push(run_id.to_owned());
            self.outcome
                .lock()
                .expect("RecordingPaneReleaser outcome mutex poisoned")
                .unwrap_or(PaneReleaseOutcome::Reaped)
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String, String)>>,
        work_events: Mutex<Vec<(String, String, String)>>,
        typed_events: Mutex<Vec<(String, boss_protocol::FrontendEvent)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, exec_id: &str, work_item_id: &str, status: &str, reason: &str) {
            self.events.lock().await.push((
                exec_id.to_owned(),
                work_item_id.to_owned(),
                status.to_owned(),
                reason.to_owned(),
            ));
        }
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
            product_id: &str,
            event: boss_protocol::FrontendEvent,
        ) {
            self.typed_events
                .lock()
                .await
                .push((product_id.to_owned(), event));
        }
    }

    /// Build a WorkDb plus a chore in `waiting_human` execution state with
    /// a cube lease attached — this is the state the engine is in once
    /// `PaneSpawnRunner::run_execution` has returned and
    /// `record_run_completion` has run.
    fn fixture(workspace_path: &Path) -> (Arc<WorkDb>, String, String, String) {
        let dir = tempdir().unwrap();
        // Box-leak the dir; tests are short-lived and this avoids
        // returning the TempDir handle.
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Detect worker stop".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        // Mirror PaneSpawnRunner: run is recorded as completed and the
        // execution sits in `waiting_human` with the lease still held.
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        (db, product.id, chore.id, execution.id)
    }

    fn ci_remediation_fixture(
        workspace_path: &Path,
    ) -> (Arc<WorkDb>, String, String, String, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Fix CI".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/88";
        db.update_work_item(
            &chore.id,
            crate::work::WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        let attempt = db
            .insert_ci_remediation(crate::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr_url.into(),
                pr_number: 88,
                head_branch: "feature".into(),
                head_sha_at_trigger: "head-1".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.mark_chore_blocked_ci_failure(&chore.id, pr_url, Some(&attempt.id))
            .unwrap();
        db.mark_ci_remediation_running(&attempt.id, "lease-1", "ws-1", "worker-1")
            .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::CiRemediation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();
        (db, product.id, chore.id, execution.id, attempt.id)
    }

    #[tokio::test]
    async fn pr_detected_moves_work_item_to_in_review_and_releases_lease() {
        let workspace = tempdir().unwrap();
        let (db, product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        // P992 task 7: chore_implementation now enqueues a reviewer and holds
        // the task in `active` until the reviewer resolves.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "expected ReviewerEnqueued; got {outcome:?}",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                // Task is held in `active` (not advanced to `in_review`) while
                // the independent reviewer pass runs.
                assert_eq!(t.status, TaskStatus::Active);
                // pr_url IS stamped so the reviewer can find the PR.
                assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "the engine must release the cube lease so the next dispatch can take it",
        );
        let publisher_events = publisher.events.lock().await.clone();
        assert!(
            publisher_events.iter().any(|(_, _, _, reason)| reason == "worker_pr_completed"),
            "expected worker_pr_completed execution event, got {publisher_events:?}",
        );
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events
                .iter()
                .any(|(p, w, reason)| p == &product_id
                    && w == &chore_id
                    && reason == "worker_pr_completed"),
            "expected work-item invalidation for the chore, got {work_events:?}",
        );
        assert_eq!(
            pane.calls.lock().await.as_slice(),
            [execution_id.as_str()],
            "pane teardown must fire on PR completion so the libghostty slot returns to Free",
        );
        assert!(
            probes.snapshot().is_empty(),
            "fresh-PR completion must NOT queue a probe — the worker is done",
        );
    }

    #[tokio::test]
    async fn on_stop_uses_staged_pr_url_and_skips_detector() {
        // Primary path: the worker ran `gh pr create` mid-run, the
        // events-socket dispatcher captured the URL into the staging
        // cache, the worker did more work, then stopped. On Stop the
        // handler must:
        //   1. read the staged URL,
        //   2. NOT invoke the detector (jj+gh reconstruction),
        //   3. transition the work item to `in_review` with the
        //      staged URL bound,
        //   4. release the lease + pane.
        //
        // The detector is wired with a deliberately-wrong URL so any
        // accidental fall-through to the cold path would be visible
        // as a wrong pr_url on the work item.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged_pr_urls.record_if_unset(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/458",
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None)));

        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds the task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/458"),
            "expected ReviewerEnqueued with staged URL, got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "the staged-URL short-circuit must skip the detector entirely (this is the whole point — no jj log, no gh api commits/{{sha}}/pulls)",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                // Held in `active` while reviewer runs; pr_url is stamped.
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/spinyfin/mono/pull/458"),
                    "the chore must bind to the STAGED URL, not the detector's wrong URL",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Cache is cleared after a successful transition so a repeat
        // Stop on the same execution wouldn't re-fire transition logic
        // against a stale entry.
        assert!(
            staged_pr_urls.get(&execution_id).is_none(),
            "staging cache must be cleared after the successful transition",
        );
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "lease release must still fire on the primary path",
        );
        assert_eq!(
            pane.calls.lock().await.as_slice(),
            [execution_id.as_str()],
            "pane teardown must still fire on the primary path",
        );
        assert!(
            probes.snapshot().is_empty(),
            "fresh-PR completion must not queue a probe",
        );
    }

    #[tokio::test]
    async fn on_stop_with_no_staged_url_still_falls_back_to_detector() {
        // Regression test for the cold path. After this PR ships,
        // the staged-URL shortcut handles 99% of cases, but the
        // detector path remains as engine-restart recovery (if the
        // engine restarted between `gh pr create` and Stop, the
        // staging cache is empty here and we must still find the
        // PR through the legacy jj+gh path).
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector =
            StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // No `with_staged_pr_urls` call — handler uses the default
        // empty cache. The detector must be invoked.
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds the task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "expected ReviewerEnqueued; got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            1,
            "with no staged URL, the detector is the only way to bind — it must be called",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/12"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recheck_for_pr_uses_staged_pr_url_and_skips_detector() {
        // Merge-poller mirror: if the on-Stop path missed staging
        // (e.g. PostToolUse arrived after Stop in the wrong order
        // because of socket reordering, or the engine restarted),
        // the merge poller's `recheck_for_pr` sweep is the second
        // chance to find the URL. Same shortcut applies — if the
        // dispatcher staged a URL between Stop and now, recheck
        // uses it without the detector.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::err("jj broken");
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged_pr_urls.record_if_unset(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/458",
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None)));

        // Detector intentionally returns Err — if recheck called it,
        // recheck would surface `DetectorFailed`. With the staged
        // shortcut, recheck must succeed without ever touching the
        // detector.
        let outcome = handler.recheck_for_pr(&execution_id).await;
        // P992 (regression fix): chore_implementation advances to in_review and
        // enqueues an async reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/458"),
            "expected ReviewerEnqueued from recheck via staged URL, got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "recheck must skip the detector when a staged URL is present",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/spinyfin/mono/pull/458"),
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recheck_for_pr_staged_url_rejected_on_branch_mismatch() {
        // T520 / T523 regression: a staged URL whose PR belongs to a
        // different execution's branch must be silently dropped, and the
        // recheck must fall through to the cold-path detector (which
        // sees no PR for the correct branch) rather than incorrectly
        // advancing the work item to in_review.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // Detector returns None → this execution has no PR yet.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged_pr_urls.record_if_unset(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/579",
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_pr_urls(staged_pr_urls.clone())
        // PR #579 belongs to a DIFFERENT execution's branch — simulate
        // the mismatch that killed T520's worker.
        .with_branch_verifier(StubBranchVerifier::ok("boss/exec_some_other_exec_id"));

        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "branch mismatch must drop the staged URL and fall through to cold path; got {outcome:?}",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, TaskStatus::Active,
                    "branch-mismatched PR must NOT advance the chore to in_review",
                );
                assert!(t.pr_url.is_none(), "branch-mismatched PR must not bind pr_url");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Staged URL must be cleared after mismatch so the next sweep
        // doesn't re-evaluate the same wrong URL.
        assert!(
            staged_pr_urls.get(&execution_id).is_none(),
            "mismatched staged URL must be evicted from the cache",
        );
        assert_eq!(
            cube.release_calls.lock().await.len(),
            0,
            "branch mismatch must NOT release the cube lease",
        );
    }

    #[tokio::test]
    async fn on_stop_staged_url_rejected_on_branch_mismatch() {
        // Defence-in-depth: the on_stop path applies the same branch
        // check as recheck_for_pr. A staged URL for a different execution's
        // PR must be dropped and fall through to the cold-path detector.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // Detector returns None → no real PR for this execution's branch.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged_pr_urls.record_if_unset(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/458",
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok("boss/exec_completely_different_id"));

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "branch-mismatched staged URL must not advance to in_review; got {outcome:?}",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, TaskStatus::Active,
                    "wrong-branch PR must NOT move chore to in_review",
                );
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(
            staged_pr_urls.get(&execution_id).is_none(),
            "mismatched staged URL must be cleared from the cache",
        );
    }

    #[tokio::test]
    async fn on_stop_staged_url_associates_prefix_divergent_branch() {
        // Issue #1145 regression: a worker that honoured a product
        // `worker_branch_prefix` (e.g. `bduff/`) opened its PR on
        // `bduff/<exec-id>`, while the engine reconstructs
        // `boss/<exec-id>` as the expected branch. The work-item suffix
        // (`exec_<id>`) is identical, so the staged URL MUST associate —
        // the whole point of the fix is that the worker no longer has to
        // close a compliant `bduff/` PR and recreate it under `boss/`.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // Cold-path detector wired with a wrong URL: any fall-through
        // would surface as a wrong pr_url, proving the staged URL was
        // (incorrectly) dropped.
        let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged_pr_urls.record_if_unset(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/458",
        );

        // The expected branch is `boss/<exec-id>` (BossExecPrefix), but
        // the PR's head branch is `bduff/<exec-id>` — same suffix, only
        // the prefix differs.
        let expected = expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None);
        let suffix = branch_work_item_suffix(&expected);
        let divergent_branch = format!("bduff/{suffix}");
        assert_ne!(divergent_branch, expected, "test must exercise a real prefix divergence");

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&divergent_branch));

        let outcome = handler.on_stop(&execution_id).await;
        // P992 (regression fix): chore_implementation advances to in_review and
        // enqueues an async reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/458"),
            "prefix-divergent but suffix-matching PR must associate; got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "the staged URL must be accepted (suffix match) and the detector skipped",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/spinyfin/mono/pull/458"),
                    "the chore must bind to the staged `bduff/` PR URL",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_absent_publishes_awaiting_pr_and_queues_probe() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        assert_eq!(outcome, StopOutcome::AwaitingInput);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active, "no PR must NOT move to in_review");
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "no PR must NOT release the cube workspace",
        );
        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
            "expected worker_awaiting_pr event for the no-PR case, got {events:?}",
        );
        assert!(
            pane.calls.lock().await.is_empty(),
            "no PR must NOT release the pane",
        );
        let queued = probes.snapshot();
        assert_eq!(
            queued.len(),
            1,
            "exactly one probe must be queued when the worker stops without a PR, got {queued:?}",
        );
        assert_eq!(queued[0].0, execution_id);
        assert_eq!(queued[0].1, PROBE_NO_PR);
    }

    #[tokio::test]
    async fn stale_pr_publishes_awaiting_pr_and_queues_push_probe() {
        // PR exists but local commits are ahead of the PR's head sha.
        // The work item must NOT move to in_review, the lease must
        // stay held, and the worker gets probed to push the missing
        // commits so the next Stop sees a fresh PR.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::Stale {
            url: "https://github.com/foo/bar/pull/42".into(),
            reason: "local HEAD abcd1234 is ahead of PR head 9876fedc".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        match outcome {
            StopOutcome::StalePr { pr_url, .. } => {
                assert_eq!(pr_url, "https://github.com/foo/bar/pull/42");
            }
            other => panic!("expected StalePr, got {other:?}"),
        }
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, TaskStatus::Active,
                    "stale PR must NOT move the work item to in_review",
                );
                assert!(t.pr_url.is_none(), "stale PR must NOT stamp pr_url yet");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());

        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
            "stale PR must publish worker_awaiting_pr, got {events:?}",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
        assert_eq!(queued[0].1, PROBE_STALE_PR);
    }

    #[tokio::test]
    async fn detector_failure_does_not_probe_worker() {
        // A transient `gh`/network failure must NOT inject a probe into the
        // worker pane.  Probing on detector failure creates a re-entrancy
        // loop: worker responds → stops → detection fails again → probe
        // again → …  The merge-poller recheck recovers the transition once
        // the failure clears.
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::err("gh broken");
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::DetectorFailed);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        let queued = probes.snapshot();
        assert!(
            queued.is_empty(),
            "detector failure must NOT probe the worker, got {queued:?}",
        );
    }

    #[tokio::test]
    async fn unknown_execution_is_a_noop() {
        let detector = StubPrDetector::ok(Some("https://github.com/x/y/pull/1"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop("not-an-execution").await;
        assert_eq!(outcome, StopOutcome::UnknownExecution);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        assert!(publisher.events.lock().await.is_empty());
        assert!(
            probes.snapshot().is_empty(),
            "unknown executions must NOT queue probes",
        );
    }

    #[tokio::test]
    async fn force_release_releases_pane_and_cube_lease_then_idempotent() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        handler.force_release(&execution_id).await;

        // First call: pane fired, cube release fired exactly once.
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        let execution = db.get_execution(&execution_id).unwrap();
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());

        // Second call: idempotent — no second cube release. The pane
        // releaser is invoked again here (the registry-level
        // idempotency lives in `WorkerRegistry::take_slot_for_run`),
        // but no extra cube release happens because the lease columns
        // are already cleared.
        handler.force_release(&execution_id).await;
        assert_eq!(
            cube.release_calls.lock().await.len(),
            1,
            "cube release must fire only once across duplicate force_release calls",
        );
    }

    #[tokio::test]
    async fn force_release_no_lease_skips_cube_release() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // Pre-clear the lease so force_release can confirm it skips
        // cube release when there's nothing to release.
        db.clear_execution_workspace(&execution_id).unwrap();

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        handler.force_release(&execution_id).await;
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert!(cube.release_calls.lock().await.is_empty());
    }

    /// T981 regression — the lease-release gate. When the pane releaser
    /// reports `NoLiveWorker` (the worker is still mid-spawn: no slot
    /// mapped, no pid to reap), `force_release` must NOT free the cube
    /// lease. Freeing it would hand a workspace the worker is about to
    /// occupy back to cube, which re-leases it into a same-workspace
    /// collision. The lease stays held until the in-flight run reaps the
    /// worker and releases it.
    #[tokio::test]
    async fn force_release_mid_spawn_holds_cube_lease() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::with_outcome(
            PaneReleaseOutcome::NoLiveWorker,
        ));
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        handler.force_release(&execution_id).await;

        // Pane release was attempted...
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        // ...but the cube lease was NOT released, and the row still
        // carries it — the still-occupied workspace stays leased.
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "mid-spawn force_release must not release the cube lease",
        );
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.cube_lease_id.as_deref(),
            Some("lease-1"),
            "lease columns must stay set so the in-flight run owns the eventual release",
        );
        assert_eq!(execution.workspace_path.as_deref(), workspace.path().to_str());
    }

    /// T981 regression — `cancel_and_release` racing the spawn window.
    /// It cancels the execution row (so the reconciler won't redispatch)
    /// but, with the worker still mid-spawn, must leave the lease held —
    /// the in-flight run reaps + releases once its spawn settles.
    #[tokio::test]
    async fn cancel_and_release_mid_spawn_cancels_row_but_holds_lease() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::with_outcome(
            PaneReleaseOutcome::NoLiveWorker,
        ));
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        handler
            .cancel_and_release(&execution_id, "test: mid-spawn cancel")
            .await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.status, ExecutionStatus::Cancelled,
            "the execution row must be cancelled so the reconciler won't redispatch it",
        );
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "the lease must stay held while the worker is still occupying the workspace",
        );
        assert_eq!(
            execution.cube_lease_id.as_deref(),
            Some("lease-1"),
            "lease columns must remain so the in-flight run can release after reaping",
        );
    }

    /// Companion to the gate test: when a live worker WAS reaped
    /// (`Reaped`), `cancel_and_release` releases the lease as before — the
    /// gate only defers on the mid-spawn case.
    #[tokio::test]
    async fn cancel_and_release_with_live_worker_releases_lease() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default()); // defaults to Reaped
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        handler
            .cancel_and_release(&execution_id, "test: live worker cancel")
            .await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Cancelled);
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert!(execution.cube_lease_id.is_none());
    }

    #[tokio::test]
    async fn duplicate_stop_after_pr_detection_is_idempotent() {
        let workspace = tempdir().unwrap();
        let (db, _, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        // P992 task 7: first Stop enqueues reviewer and holds task in active.
        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::ReviewerEnqueued { .. }
        ));
        // A second Stop event for the same execution must NOT
        // duplicate work — release is called once, work item stays
        // pinned at `active` (pending review). The pane releaser is
        // invoked again here; production releasers must be idempotent
        // on their own (see `WorkerRegistry::take_slot_for_run`).
        assert_eq!(
            handler.on_stop(&execution_id).await,
            StopOutcome::AlreadyTerminal,
        );
        assert_eq!(cube.release_calls.lock().await.len(), 1);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merged_pr_skips_in_review_and_moves_chore_to_done() {
        // The Stop arrives after the worker pushed AND the PR was
        // merged (e.g. fast-merge during the run). The detector
        // reports `Merged`; the chore must move directly to `done`
        // instead of `in_review`, the cube lease is released, and
        // the publish reason is `worker_pr_merged` so the frontend
        // can paint the right activity.
        let workspace = tempdir().unwrap();
        let (db, product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::Merged {
            url: "https://github.com/foo/bar/pull/42".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        match outcome {
            StopOutcome::PrMerged { pr_url } => {
                assert_eq!(pr_url, "https://github.com/foo/bar/pull/42");
            }
            other => panic!("expected PrMerged, got {other:?}"),
        }
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Done, "merged-at-stop must skip in_review");
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/foo/bar/pull/42"),
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.cube_lease_id.is_none());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "merged-at-stop must still release the cube lease",
        );
        let publisher_events = publisher.events.lock().await.clone();
        assert!(
            publisher_events
                .iter()
                .any(|(_, _, _, reason)| reason == "worker_pr_merged"),
            "expected worker_pr_merged execution event, got {publisher_events:?}",
        );
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events.iter().any(|(p, w, reason)| p == &product_id
                && w == &chore_id
                && reason == "worker_pr_merged"),
            "expected work-item invalidation tagged worker_pr_merged, got {work_events:?}",
        );
        assert!(
            probes.snapshot().is_empty(),
            "merged-at-stop must NOT queue a probe — the worker is done",
        );
    }

    #[tokio::test]
    async fn closed_unmerged_pr_treated_as_no_pr() {
        // PR was closed without merging — work shouldn't advance to
        // `in_review` or `done`. Behave like the no-PR case so the
        // worker is asked to confirm what they want.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::Closed {
            url: "https://github.com/foo/bar/pull/9".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(cube.release_calls.lock().await.is_empty());
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1, PROBE_NO_PR);
    }

    /// Build a `kind = 'conflict_resolution'` execution against a chore
    /// that is currently `blocked: merge_conflict`. Also inserts the
    /// matching `conflict_resolutions` row in `running` so the
    /// completion finalizer has something to look up. Mirrors the
    #[tokio::test]
    async fn non_conflict_kind_execution_does_not_invoke_finalizer() {
        // The standard chore_implementation kind must NOT trip the
        // conflict-resolution finalizer even if a conflict_resolutions
        // row happens to exist for the same work item (e.g. a prior
        // attempt was archived).
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // Pre-existing failed attempt unrelated to this execution.
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: "any".into(),
                work_item_id: chore_id.clone(),
                pr_url: "https://github.com/foo/bar/pull/99".into(),
                pr_number: 99,
                head_branch: "x".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("bsha".into()),
                head_sha_before: None,
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "lease-x", "ws-x", "worker-x")
            .unwrap();
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube,
            publisher.clone(),
            pane,
            probes,
        );
        let _ = handler.on_stop(&execution_id).await;

        // The chore_implementation execution must not touch the
        // sibling conflict_resolutions row; if it did, the attempt
        // would now be `failed` instead of `running`.
        let after = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
        assert_eq!(
            after.status, "running",
            "non-conflict-kind executions must not trip the conflict-resolution finalizer",
        );
    }

    #[tokio::test]
    async fn cross_execution_attribution_uses_per_execution_branch_name() {
        let alice_ws = tempdir().unwrap();
        let bob_ws = tempdir().unwrap();
        let (db, _alice_product, _alice_chore, alice_exec) = fixture(alice_ws.path());
        // Fresh DB for Bob so the two executions are independent —
        // we're modelling them as living in different cube
        // workspaces, not contending for the same chore.
        let (bob_db, _bob_product, _bob_chore, bob_exec) = fixture(bob_ws.path());

        // Detector returns Fresh URLs unique per branch — the
        // production behaviour of `gh pr list --head <branch>` once
        // each worker has pushed.
        struct PerBranchDetector;
        #[async_trait]
        impl PrDetector for PerBranchDetector {
            async fn detect_pr(
                &self,
                _repo_remote_url: &str,
                expected_branch: &str,
            ) -> Result<PrStatus> {
                Ok(PrStatus::Fresh {
                    url: format!("https://github.com/spinyfin/mono/pull/PR-for-{expected_branch}"),
                })
            }
        }
        let detector = Arc::new(PerBranchDetector);

        let alice_handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        );
        let bob_handler = WorkerCompletionHandler::new(
            bob_db.clone(),
            detector,
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        );

        let alice_outcome = alice_handler.on_stop(&alice_exec).await;
        let bob_outcome = bob_handler.on_stop(&bob_exec).await;

        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        let alice_url = match alice_outcome {
            StopOutcome::ReviewerEnqueued { pr_url } => pr_url,
            other => panic!("alice expected ReviewerEnqueued, got {other:?}"),
        };
        let bob_url = match bob_outcome {
            StopOutcome::ReviewerEnqueued { pr_url } => pr_url,
            other => panic!("bob expected ReviewerEnqueued, got {other:?}"),
        };
        assert_ne!(
            alice_url, bob_url,
            "two concurrent workers in different workspaces must bind to different PRs — \
             the fan-out bug from incident 001 was exactly the case where they got the same one",
        );
        assert!(
            alice_url.contains(&expected_branch_name(&alice_exec, &BranchNaming::BossExecPrefix, None)),
            "alice's bound URL must derive from her own execution id, got {alice_url}",
        );
        assert!(
            bob_url.contains(&expected_branch_name(&bob_exec, &BranchNaming::BossExecPrefix, None)),
            "bob's bound URL must derive from his own execution id, got {bob_url}",
        );
    }

    /// Seed a chore + execution left in `waiting_human` (the lease still
    /// held), occupying cube workspace `workspace_id`. Returns
    /// `(chore_id, execution_id)`. Mirrors the `fixture` lifecycle but
    /// lets a test place several occupants in one workspace.
    fn seed_workspace_occupant(
        db: &Arc<WorkDb>,
        product_id: &str,
        name: &str,
        lease: &str,
        workspace_id: &str,
        workspace_path: &str,
    ) -> (String, String) {
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: name.to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: true,
            })
            .unwrap();
        let exec = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (_e, run) = db
            .start_execution_run(&exec.id, "worker", "mono", lease, workspace_id, workspace_path)
            .unwrap();
        db.finish_execution_run(
            &exec.id,
            &run.id,
            ExecutionStatus::WaitingHuman,
            "completed",
            Some("spawned worker pane"),
            None,
            false,
            None,
        )
        .unwrap();
        (chore.id, exec.id)
    }

    /// Reused-workspace stale-Stop guard (the bug this change fixes):
    /// two executions occupy the same warm-cached cube workspace. The
    /// older is a stale prior occupant whose `boss-event` Stop hook
    /// leaked from a settings.json left in the re-leased tree. Its Stop
    /// must be ignored — not finalized, no lease released — while the
    /// live (newest) execution's own Stop still completes it.
    #[tokio::test]
    async fn stale_stop_from_superseded_workspace_occupant_is_ignored() {
        let ws = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        let ws_path = ws.path().to_str().unwrap();
        let (stale_chore, stale_exec) =
            seed_workspace_occupant(&db, &product.id, "stale", "lease-stale", "mono-agent-shared", ws_path);
        let (_live_chore, live_exec) =
            seed_workspace_occupant(&db, &product.id, "live", "lease-live", "mono-agent-shared", ws_path);

        // Force deterministic recency: the stale occupant created
        // earlier. (`created_at` is second-granularity, so two rows
        // created in the same test tick would otherwise tie.)
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET created_at = '100' WHERE id = ?1",
                rusqlite::params![stale_exec],
            )
            .unwrap();
            conn.execute(
                "UPDATE work_executions SET created_at = '200' WHERE id = ?1",
                rusqlite::params![live_exec],
            )
            .unwrap();
        }

        let cube = Arc::new(StubCubeClient::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/910")),
            cube.clone(),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        );

        // The stale occupant's leaked Stop must be ignored.
        assert_eq!(
            handler.on_stop(&stale_exec).await,
            StopOutcome::SupersededInWorkspace,
            "a stale Stop from a superseded reused-workspace occupant must be ignored",
        );
        // Its chore is not pushed to in_review and no lease is released.
        match db.get_work_item(&stale_chore).unwrap() {
            WorkItem::Chore(t) => assert_ne!(
                t.status, TaskStatus::InReview,
                "the stale occupant's task must not transition on a leaked Stop",
            ),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "a leaked stale Stop must not release any cube lease",
        );

        // P992 task 7: the live execution's Stop enqueues a reviewer and
        // holds the live chore in active.
        assert!(
            matches!(handler.on_stop(&live_exec).await, StopOutcome::ReviewerEnqueued { .. }),
            "the live execution's own Stop must still complete it",
        );
    }

    /// AI #6 running-status gate: if the Stop hook fires on an
    /// execution that's still in `running` status (i.e. the worker is
    /// alive and racing through turns) and there's no staged URL, the
    /// fallback MUST NOT fire. Pre-incident-001 it did, and the
    /// per-turn firing rate against cube's shared `.jj/repo/store/git`
    /// is what produced the May 14 fan-out.
    /// Build a fixture left in `running` status — i.e. `start_execution_run`
    /// has fired but `finish_execution_run` has not yet been called. The
    /// in-cube worker pane is alive, and a `Stop` hook fires for the
    /// first assistant turn before the upper layer has had a chance to
    /// stamp `waiting_human`.
    fn fixture_running(workspace_path: &Path) -> (Arc<WorkDb>, String, String, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Running execution".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        // `start_execution_run` flips the row to `running`. Do not
        // follow up with `finish_execution_run` — we want the row to
        // stay in `running` to exercise the AI #6 gate.
        let (execution, _run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        assert_eq!(execution.status, ExecutionStatus::Running);
        (db, product.id, chore.id, execution.id)
    }

    #[tokio::test]
    async fn running_status_short_circuits_without_calling_detector() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture_running(workspace.path());

        let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::RunningNoStagedPr,
            "running execution with no staged URL must short-circuit, not invoke the detector",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "running-status gate must not call detect_pr",
        );
        // Chore stays put, no probe queued, no publish.
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        assert!(probes.snapshot().is_empty());
        assert!(publisher.events.lock().await.is_empty());
    }

    /// Companion to the running-status gate test: when the execution
    /// IS in `waiting_human` (worker has paused and is awaiting human
    /// review), the fallback fires. This is the only state in which
    /// the cold path is allowed to run, per the incident-001 fix.
    #[tokio::test]
    async fn waiting_human_status_invokes_detector_with_expected_branch() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
        // Fixture leaves the execution in `waiting_human`; the on-Stop
        // handler should fall through to the detector.
        let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/501"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "expected ReviewerEnqueued; got {outcome:?}",
        );
        assert_eq!(detector.call_count(), 1);
        let calls = detector.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].expected_branch,
            expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None),
            "detect_pr must be invoked with the execution's deterministic branch name",
        );
    }

    /// Integration: the handler must publish `awaiting_pr` and queue
    /// `PROBE_EMPTY_PR` when the detector reports `EmptyDiff`. The
    /// work item must stay in `active` and the cube lease must NOT
    /// be released — the worker is alive and must fix its PR first.
    #[tokio::test]
    async fn empty_diff_pr_publishes_awaiting_pr_and_queues_empty_pr_probe() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::EmptyDiff {
            url: "https://github.com/foo/bar/pull/77".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        match outcome {
            StopOutcome::EmptyDiffPr { pr_url } => {
                assert_eq!(pr_url, "https://github.com/foo/bar/pull/77");
            }
            other => panic!("expected EmptyDiffPr, got {other:?}"),
        }
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, TaskStatus::Active,
                    "empty-diff PR must NOT move the work item to in_review",
                );
                assert!(t.pr_url.is_none(), "empty-diff PR must NOT stamp pr_url");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "empty-diff PR must NOT release the cube workspace",
        );
        assert!(pane.calls.lock().await.is_empty(), "empty-diff PR must NOT release the pane");

        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
            "empty-diff PR must publish worker_awaiting_pr, got {events:?}",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
        assert_eq!(queued[0].0, execution_id);
        assert_eq!(queued[0].1, PROBE_EMPTY_PR);
    }

    /// End-to-end regression mirror of the user-reported symptom:
    /// the chore description references multiple historical merged
    /// PRs in narrative text, and the worker exits without pushing.
    /// The detector returns `None` (because the structural head-sha
    /// gate in `classify_pr` rejects the parent-on-main false
    /// positive), and the on-Stop handler must therefore leave the
    /// chore in `active` with `pr_url` unset — NOT transition it to
    /// `done` against one of the PRs mentioned in the description.
    ///
    /// We can't drive `classify_pr` from end-to-end here without a
    /// real `gh`/`jj` install in the test harness, so we stub the
    /// detector with `PrStatus::None` (the exact value the fixed
    /// `classify_pr` now returns for the bug scenario) and assert on
    /// the chore-and-execution state the handler is supposed to land
    /// in. The description-text storm is preserved to make the
    /// intent clear if this test ever has to be revisited.
    #[tokio::test]
    async fn chore_with_pr_references_in_description_stays_active_when_worker_exits_without_pr() {
        let workspace = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let description_with_pr_refs = "\
This is a follow-up to PR #379 (the auto-bind safety net work). \
See #379 for context. We also referenced #379 in the design doc. \
PR #379 was reverted in #381; the structural fix from PR #379 should \
not be reintroduced as-is. Out-of-scope section of prior PR #379 \
applies. Discussion in PR #379 still relevant. PR #379. PR #379. \
PR #379. PR #379.";
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Engine PR-auto-bind regression returned".into(),
                description: Some(description_with_pr_refs.into()),
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        // Worker exited without pushing — the (now-fixed) detector
        // returns `None` rather than misbinding to one of the PRs
        // mentioned in the description text.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution.id).await;
        assert_eq!(outcome, StopOutcome::AwaitingInput);

        let item = db.get_work_item(&chore.id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, TaskStatus::Active,
                    "chore with PR refs in description must stay active when the worker exits without a PR",
                );
                assert!(
                    t.pr_url.is_none(),
                    "pr_url must NOT be stamped from description text — got {:?}",
                    t.pr_url,
                );
                assert_ne!(
                    t.last_status_actor, "engine",
                    "engine must NOT be the last status actor when no PR was bound",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }

        let exec_after = db.get_execution(&execution.id).unwrap();
        assert_eq!(
            exec_after.status, ExecutionStatus::WaitingHuman,
            "execution must stay in waiting_human so a follow-up Stop can re-check",
        );
        assert_eq!(
            exec_after.cube_lease_id.as_deref(),
            Some("lease-1"),
            "cube lease must NOT be released when no PR was bound",
        );

        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
        assert_eq!(queued[0].1, PROBE_NO_PR);
    }

    /// Second half of the required coverage: a chore whose
    /// description references multiple historical PRs, but the
    /// worker actually pushes and creates a real PR. The detector
    /// reports `Fresh { url }` for the worker's PR. The chore must
    /// bind to *that* PR (the one the worker actually created), not
    /// to any of the PRs mentioned in the description text.
    #[tokio::test]
    async fn chore_with_pr_references_in_description_binds_to_worker_created_pr_not_description_pr()
    {
        let workspace = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        // Description points at PR #379 repeatedly as prior art. The
        // worker is going to actually create PR #500.
        let description_with_pr_refs = "\
Follow-up to PR #379. See PR #379. Reverted in #381. PR #379. PR #379. \
PR #379. PR #379. PR #379. PR #379. PR #379.";
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Engine PR-auto-bind regression returned".into(),
                description: Some(description_with_pr_refs.into()),
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        // The worker DID create a real PR — number 500, freshly
        // opened. The (fixed) detector reports that fresh PR's url,
        // NOT any of the description-mentioned PRs.
        let workers_actual_pr = "https://github.com/spinyfin/mono/pull/500";
        let description_mentioned_pr = "https://github.com/spinyfin/mono/pull/379";
        let detector = StubPrDetector::ok(Some(workers_actual_pr));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube,
            publisher,
            pane,
            probes,
        );
        let outcome = handler.on_stop(&execution.id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        match outcome {
            StopOutcome::ReviewerEnqueued { pr_url } => {
                assert_eq!(
                    pr_url, workers_actual_pr,
                    "must bind to the worker-created PR, not the description-mentioned one",
                );
            }
            other => panic!("expected ReviewerEnqueued, got {other:?}"),
        }

        let item = db.get_work_item(&chore.id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some(workers_actual_pr),
                    "pr_url must be the worker's actual PR",
                );
                assert_ne!(
                    t.pr_url.as_deref(),
                    Some(description_mentioned_pr),
                    "pr_url MUST NOT be one of the historical PRs the description mentions",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Regression for the missed PR-open detection that left chore
    /// `task_18aefd1f955e5348_e` (PR #415) stuck in `active` with
    /// `pr_url=NULL`. The on-Stop hook can miss a freshly-opened PR
    /// when GitHub's `commits/{sha}/pulls` index hasn't caught up yet
    /// (PR #415 was created 7s before the Stop fired). When that
    /// happens the chore stays `active`, the merge poller's primary
    /// query (`list_chores_pending_merge_check`) never picks it up
    /// (that query gates on `status='in_review'`), and the chore is
    /// stuck. The fix routes `waiting_human` executions with no
    /// `pr_url` through `WorkerCompletionHandler::recheck_for_pr` on
    /// every merge-poller pass, so a delayed GitHub-side propagation
    /// recovers on the next 60s sweep.
    #[tokio::test]
    async fn merge_poller_recovers_missed_pr_open_for_waiting_human_execution() {
        use crate::merge_poller::{
            MergeProbe, OpenPrStatus, PrLifecycleProbe, PrLifecycleState,
        };

        // Fixture leaves the chore in `active` and the execution in
        // `waiting_human` with a workspace_path — exactly the state
        // PR #415 was in after its on-Stop hook missed.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());

        // Simulate "on-Stop already ran and saw no PR" by leaving
        // the chore's pr_url unset. The recheck path is what we're
        // testing — it must see PrStatus::Fresh on this pass and
        // promote the chore.
        let workers_pr = "https://github.com/foo/bar/pull/415";
        let detector = StubPrDetector::ok(Some(workers_pr));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = Arc::new(WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        ));

        // Wire a no-op MergeProbe — the test exercises only the
        // pending-PR-detection arm of the sweep, not the in-review
        // merge path.
        struct NoOpProbe;
        #[async_trait]
        impl MergeProbe for NoOpProbe {
            async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
                Ok(PrLifecycleProbe {
                    url: String::new(),
                    state: PrLifecycleState::Open(OpenPrStatus::clean()),
                    base_ref_oid: None,
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: crate::merge_poller::PrReviewState::Unknown,
                    in_merge_queue: false,
                })
            }
        }
        let probe = NoOpProbe;

        let outcome = crate::merge_poller::run_one_pass(
            db.as_ref(),
            &probe,
            publisher.as_ref(),
            None,
            Some(handler.as_ref()),
        )
        .await;

        assert_eq!(
            outcome.pr_recheck_recovered, 1,
            "the sweep must recover exactly one missed PR-open transition, got {outcome:?}",
        );

        // P992 task 7: chore held in `active` with pr_url stamped
        // (reviewer is enqueued to run the review pass).
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(t.pr_url.as_deref(), Some(workers_pr));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Execution finalised — lease released, pane torn down.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.cube_lease_id.is_none());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "the recovery must release the cube lease just like the on-Stop path does",
        );
        assert_eq!(
            pane.calls.lock().await.as_slice(),
            [execution_id.as_str()],
            "the recovery must tear down the pane just like the on-Stop path does",
        );
        // Crucially: NO probe was queued. Periodic polling must not
        // spam the worker's probe FIFO.
        assert!(
            probes.snapshot().is_empty(),
            "merge-poller recovery must NOT queue a probe — that's a Stop-event side effect only",
        );
        // Publish reason distinguishes recheck from on-Stop so
        // operators can see which path closed the chore.
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events
                .iter()
                .any(|(_, _, r)| r == "worker_pr_completed_recheck"),
            "expected worker_pr_completed_recheck publish reason, got {work_events:?}",
        );
    }

    /// Periodic polling must NOT queue probes when the detector
    /// still sees no PR — that's the side effect that makes the
    /// no-PR branch of `on_stop` correct but the no-PR branch of a
    /// 60s poll wrong (a Stop event happens once; a poll happens
    /// every minute).
    #[tokio::test]
    async fn recheck_for_pr_is_quiet_when_detector_still_reports_no_pr() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert_eq!(outcome, StopOutcome::AwaitingInput);

        // Chore stays where it was.
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // No probe queued, no awaiting-input event published, no
        // lease released, no pane torn down.
        assert!(
            probes.snapshot().is_empty(),
            "recheck must NOT queue probes on the no-PR branch",
        );
        assert!(
            publisher.events.lock().await.is_empty(),
            "recheck must NOT publish awaiting-input events on the no-PR branch",
        );
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
    }

    /// Sibling regression: when the detector returns PrStatus::Stale
    /// (PR exists but local commits are ahead), the recheck must
    /// also stay silent — the worker has stopped, so probing it
    /// every 60s with PROBE_STALE_PR would spam its input FIFO.
    #[tokio::test]
    async fn recheck_for_pr_is_quiet_on_stale_pr() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::Stale {
            url: "https://github.com/foo/bar/pull/42".into(),
            reason: "local HEAD ahead of PR head".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.recheck_for_pr(&execution_id).await;
        match outcome {
            StopOutcome::StalePr { .. } => {}
            other => panic!("expected StalePr, got {other:?}"),
        }
        assert!(
            probes.snapshot().is_empty(),
            "recheck must NOT queue probes on the stale-PR branch",
        );
        assert!(publisher.events.lock().await.is_empty());
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
    }

    /// Regression for the 2026-05-13 five-concurrent-workers failure
    /// (Worf/Crusher/Troi in the first wave at 21:01-21:04Z, then
    /// Yar/Riker in the second wave at 22:13-22:15Z). All five pushed
    /// real PRs but `pr_url` was never bound and their chores stayed
    /// in `active` indefinitely. The failure was invisible in engine
    /// logs because `merge_poller::sweep_pending_pr` silently returned
    /// from the `StalePr`/`AwaitingInput`/`DetectorFailed`/`EmptyDiffPr`
    /// arms.
    ///
    /// This test pins TWO contracts:
    ///
    /// 1. **Observability:** when the detector still returns Stale
    ///    (e.g. an execution without `started_at`, or some other
    ///    candidate-set failure the bookmark expansion doesn't catch),
    ///    the recheck path counts unresolved candidates on
    ///    `SweepOutcome.pr_recheck_unresolved` so a stuck worker leaves
    ///    a breadcrumb on every 60s sweep instead of failing silently.
    ///
    /// 2. **Behavioural fix:** when the detector returns `Fresh` on a
    ///    subsequent pass — the production path for this is
    ///    `jj_candidate_commit_shas` finding the worker's pushed
    ///    bookmark tip via `committer_date(after:"<started_at>")` — the
    ///    recheck binds `pr_url` and transitions the chore to
    ///    `in_review`. Without this leg, the diagnostic-only fix from
    ///    the previous dispatch left the bug live and demanded
    ///    coordinator backfill on every dispatch wave.
    #[tokio::test]
    async fn merge_poller_recheck_binds_three_stuck_workers_when_detector_recovers() {
        use crate::merge_poller::{MergeProbe, PrLifecycleProbe, PrLifecycleState};

        // Three independent workspaces / chores / executions in
        // `waiting_human` with `pr_url=null`. Mirrors the 3-worker
        // dispatch wave (Worf/Crusher/Troi).
        let ws1 = tempdir().unwrap();
        let ws2 = tempdir().unwrap();
        let ws3 = tempdir().unwrap();
        let (db, _p1, c1, e1) = fixture(ws1.path());
        // Reuse the same DB for the next two so a single merge-poller
        // pass sees all three executions.
        let chore2 = db
            .create_chore(crate::work::CreateChoreInput {
                product_id: {
                    let item = db.get_work_item(&c1).unwrap();
                    work_item_product_id(&item)
                },
                name: "Crusher".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let exec2 = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore2.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (exec2, run2) = db
            .start_execution_run(
                &exec2.id,
                "worker-2",
                "mono",
                "lease-2",
                "mono-agent-002",
                ws2.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &exec2.id,
                &run2.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                None,
                None,
                false,
                None,
            )
            .unwrap();
        let chore3 = db
            .create_chore(crate::work::CreateChoreInput {
                product_id: {
                    let item = db.get_work_item(&c1).unwrap();
                    work_item_product_id(&item)
                },
                name: "Troi".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let exec3 = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore3.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build())
            .unwrap();
        let (exec3, run3) = db
            .start_execution_run(
                &exec3.id,
                "worker-3",
                "mono",
                "lease-3",
                "mono-agent-003",
                ws3.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &exec3.id,
                &run3.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                None,
                None,
                false,
                None,
            )
            .unwrap();

        // Detector that returns Stale for every candidate — simulates
        // the failure mode where the worker's `@`/`@-` drifted from
        // the PR's head after push (`jj new main` after `jj git push`).
        // Keep a handle on the concrete stub so pass 2 can swap the
        // result without rebuilding the handler.
        let detector = StubPrDetector::ok_status(PrStatus::Stale {
            url: "https://github.com/spinyfin/mono/pull/433".into(),
            reason: "local commits do not match PR head abc1234".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        struct NoOpProbe;
        #[async_trait]
        impl MergeProbe for NoOpProbe {
            async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
                Ok(PrLifecycleProbe {
                    url: String::new(),
                    state: PrLifecycleState::Open(
                        crate::merge_poller::OpenPrStatus::clean(),
                    ),
                    base_ref_oid: None,
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: crate::merge_poller::PrReviewState::Unknown,
                    in_merge_queue: false,
                })
            }
        }
        let probe = NoOpProbe;

        let outcome = crate::merge_poller::run_one_pass(
            db.as_ref(),
            &probe,
            publisher.as_ref(),
            None,
            Some(&handler),
        )
        .await;

        // Pass 1 — pre-fix behaviour: the recheck reaches all three
        // candidates but the detector still returns Stale on each. The
        // observability counter fires so the failure leaves a
        // breadcrumb on every sweep, but nothing transitions — exactly
        // the 2026-05-13 stuck-worker shape.
        assert_eq!(
            outcome.pr_recheck_unresolved, 3,
            "the sweep must count three unresolved recheck candidates, got {outcome:?}",
        );
        assert_eq!(
            outcome.pr_recheck_recovered, 0,
            "no transitions happen on the StalePr branch",
        );
        for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
            let item = db.get_work_item(chore_id).unwrap();
            match item {
                WorkItem::Chore(t) => {
                    assert_eq!(t.status, TaskStatus::Active);
                    assert!(t.pr_url.is_none());
                }
                other => panic!("expected chore, got {other:?}"),
            }
        }
        for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
            let execution = db.get_execution(execution_id).unwrap();
            assert_eq!(
                execution.status, ExecutionStatus::WaitingHuman,
                "execution must stay in waiting_human after a Stale recheck",
            );
            assert!(
                execution.cube_lease_id.is_some(),
                "cube lease must NOT be released on the Stale branch — the worker is still alive",
            );
        }
        assert!(
            probes.snapshot().is_empty(),
            "recheck path stays quiet on Stale — no probes queued",
        );

        // Pass 2 — fix engaged: simulate the production path where
        // `jj_candidate_commit_shas`'s `committer_date(after:"<started_at>")`
        // gate has now expanded the candidate set with the worker's
        // pushed bookmark tip, so `gh api commits/{sha}/pulls` returns
        // a PR whose `head.sha` matches a local sha and `classify_pr`
        // accepts it as `Fresh`. The stub detector swaps to mirror
        // that real-world transition. All three stuck chores must
        // bind their `pr_url` and transition to `in_review` on this
        // pass — without coordinator backfill.
        *detector.result.lock().await = Ok(PrStatus::Fresh {
            url: "https://github.com/spinyfin/mono/pull/433".into(),
        });
        let outcome2 = crate::merge_poller::run_one_pass(
            db.as_ref(),
            &probe,
            publisher.as_ref(),
            None,
            Some(&handler),
        )
        .await;
        assert_eq!(
            outcome2.pr_recheck_recovered, 3,
            "all three stuck workers must transition on the recovery pass, got {outcome2:?}",
        );
        assert_eq!(
            outcome2.pr_recheck_unresolved, 0,
            "no candidates should remain unresolved after the recovery pass",
        );
        // P992 task 7: chore_implementation holds tasks in `active` while
        // reviewers are enqueued (not advanced to in_review yet).
        for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
            let item = db.get_work_item(chore_id).unwrap();
            match item {
                WorkItem::Chore(t) => {
                    assert_eq!(
                        t.status, TaskStatus::Active,
                        "chore {chore_id} must be held in active (reviewer enqueued)",
                    );
                    assert_eq!(
                        t.pr_url.as_deref(),
                        Some("https://github.com/spinyfin/mono/pull/433"),
                        "chore {chore_id} must have pr_url bound on the recovery pass",
                    );
                }
                other => panic!("expected chore, got {other:?}"),
            }
        }
        for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
            let execution = db.get_execution(execution_id).unwrap();
            assert_eq!(
                execution.status, ExecutionStatus::Completed,
                "execution {execution_id} must finalise on the recovery pass",
            );
            assert!(
                execution.cube_lease_id.is_none(),
                "cube lease must be released on the recovery pass — worker has stopped",
            );
        }
        // Three leases released, three panes torn down: one per worker.
        assert_eq!(cube.release_calls.lock().await.len(), 3);
        assert_eq!(pane.calls.lock().await.len(), 3);
        // No probes queued even on the success path — recheck never
        // probes (that's a Stop-event-only side effect).
        assert!(probes.snapshot().is_empty());
    }

    #[test]
    fn parse_repo_slug_handles_ssh_https_and_trailing_dotgit() {
        assert_eq!(
            parse_repo_slug("git@github.com:spinyfin/mono.git").unwrap(),
            "spinyfin/mono",
        );
        assert_eq!(
            parse_repo_slug("https://github.com/spinyfin/mono.git").unwrap(),
            "spinyfin/mono",
        );
        assert_eq!(
            parse_repo_slug("https://github.com/spinyfin/mono").unwrap(),
            "spinyfin/mono",
        );
        assert_eq!(
            parse_repo_slug("https://github.com/spinyfin/mono/").unwrap(),
            "spinyfin/mono",
        );
        // Anything not on github.com is rejected — we don't have a
        // generic resolver for self-hosted GitHub Enterprise yet, so
        // surfacing an explicit error keeps the failure mode obvious.
        assert!(parse_repo_slug("git@gitlab.com:foo/bar.git").is_err());
        assert!(parse_repo_slug("https://github.com/spinyfin").is_err());
    }

    /// AI #5 (incident 001): when the `detect_pr_cold_fallback` feature
    /// flag is OFF the cold-path fallback must not call the detector
    /// even for a `waiting_human` execution with an empty staged-URL
    /// cache. The outcome is the new quiet `FallbackDisabledByFlag`,
    /// no probe gets queued, the work item stays at its pre-Stop
    /// state, and the lease/pane are NOT torn down — the human is
    /// the next actor.
    #[tokio::test]
    async fn on_stop_skips_detector_when_feature_flag_is_off() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // Detector wired with a deliberately-wrong URL so any
        // accidental fall-through would surface as a wrong pr_url on
        // the chore.
        let detector =
            StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let flags_dir = tempdir().unwrap();
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            flags_dir.path().join("feature-flags.toml"),
        ));
        flags.load().unwrap();
        flags.set("detect_pr_cold_fallback", false).unwrap();

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_feature_flags(flags.clone());

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::FallbackDisabledByFlag);
        assert_eq!(
            detector.call_count(),
            0,
            "feature-flag gate must short-circuit before the detector is consulted",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                // Chore stays put — no transition to `in_review` or anything else.
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.status, ExecutionStatus::WaitingHuman,
            "execution must remain `waiting_human` for the human to resolve",
        );
        assert!(
            execution.cube_lease_id.is_some(),
            "cube lease must be retained — the human may want to re-enter the workspace",
        );
        assert!(
            pane.calls.lock().await.is_empty(),
            "pane teardown must NOT fire when the fallback is suppressed by flag",
        );
        assert!(
            probes.snapshot().is_empty(),
            "no probe must be queued — the human is the next actor",
        );
    }

    /// AI #5 mirror: the merge-poller's `recheck_for_pr` sweep must
    /// honour the same flag. The poller fires every ~60s, so a stuck
    /// `detect_pr_cold_fallback=false` setting must keep the
    /// fallback off on every sweep, not just the on-Stop path.
    #[tokio::test]
    async fn recheck_for_pr_skips_detector_when_feature_flag_is_off() {
        let workspace = tempdir().unwrap();
        let (_db_product_id, execution_id, detector, cube, publisher, pane, probes, db) = {
            let (db, product_id, _chore_id, execution_id) = fixture(workspace.path());
            let detector =
                StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
            let cube = Arc::new(StubCubeClient::default());
            let publisher = Arc::new(RecordingPublisher::default());
            let pane = Arc::new(RecordingPaneReleaser::default());
            let probes = Arc::new(RecordingProbeQueuer::default());
            (
                product_id, execution_id, detector, cube, publisher, pane, probes, db,
            )
        };

        let flags_dir = tempdir().unwrap();
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            flags_dir.path().join("feature-flags.toml"),
        ));
        flags.load().unwrap();
        flags.set("detect_pr_cold_fallback", false).unwrap();

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_feature_flags(flags.clone());

        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert_eq!(outcome, StopOutcome::FallbackDisabledByFlag);
        assert_eq!(detector.call_count(), 0);
    }

    /// Default-ON safety contract: with NO override file and no
    /// explicit wiring (the typical test path), `detect_pr` MUST still
    /// fire. This guards against a future regression where the flag's
    /// default flips off by accident — the change would show up here
    /// as the test going green on the wrong branch.
    #[tokio::test]
    async fn on_stop_calls_detector_when_feature_flag_defaults_on() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let flags_dir = tempdir().unwrap();
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            flags_dir.path().join("feature-flags.toml"),
        ));
        flags.load().unwrap(); // missing file → registry default (true)

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_feature_flags(flags);

        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "default-ON must still let `detect_pr` fire; got {outcome:?}",
        );
        assert_eq!(detector.call_count(), 1);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    // -----------------------------------------------------------
    // Resume-bounce SHA-delta gate regressions.
    //
    // Reproduces the nudge-loop bug: when a chore was bounced back
    // to a worker that already had a PR bound (`chore.pr_url`
    // populated), the cold-path detector kept missing the bound PR
    // (it searches by `boss/<execution_id>` branch, which is a
    // FRESH name for the resume execution but the worker correctly
    // pushed to the OLD branch where the PR lives). That false
    // miss queued `PROBE_NO_PR`, the worker explained "PR exists",
    // the runtime nudged again — loop.
    //
    // The fix: when `chore.pr_url` is bound, ignore the cold-path
    // detector and verify contribution via SHA delta on the bound
    // PR's head ref instead. The tests below pin three cases:
    //   1. Resume + push (head moved) → no probe, chore finalized.
    //   2. Resume + no push (head same) → probe fires.
    //   3. No bound PR → existing branch detector still runs
    //      (new-PR flow preserved).
    // -----------------------------------------------------------

    /// Variant of [`fixture`] that mirrors a resume bounce-back: the
    /// chore already carries a `pr_url` (set by an earlier run's
    /// on-Stop machinery), and the new execution has its
    /// `pr_head_before` snapshot already persisted (the equivalent of
    /// `on_execution_started` having run at dispatch time).
    fn resume_fixture(
        workspace_path: &Path,
        bound_pr_url: &str,
        head_before: &str,
    ) -> (Arc<WorkDb>, String, String, String) {
        let (db, product_id, chore_id, execution_id) = fixture(workspace_path);
        db.update_work_item(
            &chore_id,
            crate::work::WorkItemPatch {
                pr_url: Some(bound_pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        db.set_execution_pr_head_before(&execution_id, head_before)
            .unwrap();
        (db, product_id, chore_id, execution_id)
    }

    #[tokio::test]
    async fn resume_push_to_bound_pr_finalizes_without_nudge() {
        // T495-style scenario: chore already had PR 606 bound from a
        // prior run, this run pushed a fix commit so the bound PR's
        // head moved during the run. The cold-path detector would
        // miss the PR (it searches by the new execution's branch
        // name, not the OLD branch where the PR lives) — that's the
        // bug. The SHA-delta gate must intervene and finalize.
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/606";
        let head_before = "1111111111111111111111111111111111111111";
        let (db, product_id, chore_id, execution_id) =
            resume_fixture(workspace.path(), pr_url, head_before);
        // Cold-path detector reports None — this is what the live
        // engine sees on a resume because the detector searches by
        // `boss/<new-execution-id>`, which has no PR.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let verifier = StubBranchVerifier::ok("boss/exec_old");
        // Worker pushed a fix commit: head SHA moved.
        verifier
            .set_head_oid(Ok("2222222222222222222222222222222222222222".into()))
            .await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/606"),
            "SHA-delta gate must finalize the bound PR when the head moved; got {outcome:?}",
        );
        assert!(
            probes.snapshot().is_empty(),
            "no probe must fire when the bound PR moved during this run; saw {:?}",
            probes.snapshot()
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                // Task held in active (reviewer enqueued); pr_url stamped.
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(t.pr_url.as_deref(), Some(pr_url));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.finished_at.is_some());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "cube lease must be released after the SHA-delta finalize"
        );
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events.iter().any(|(p, w, _)| p == &product_id && w == &chore_id),
            "work-item invalidation must fire for the chore",
        );
    }

    #[tokio::test]
    async fn resume_without_push_to_bound_pr_still_probes() {
        // Resume bounce-back where the worker exited without pushing
        // any commit. The gate must NOT swallow this case — the
        // loop-catch nudge is load-bearing for genuinely-idle workers.
        //
        // A PR is already bound, though, so the nudge must point the
        // worker at the *existing* PR (never `gh pr create`): this is
        // the T541/T686 defect family. The first nudge fires under the
        // circuit-breaker cap.
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/606";
        let head = "1111111111111111111111111111111111111111";
        let (db, _product_id, chore_id, execution_id) =
            resume_fixture(workspace.path(), pr_url, head);
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let verifier = StubBranchVerifier::ok("boss/exec_old");
        // Head SHA matches the snapshot — worker didn't push.
        verifier.set_head_oid(Ok(head.into())).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "unchanged head SHA means no contribution; probe must fire"
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].1,
            probe_push_to_existing_pr(pr_url),
            "bound PR exists: nudge must target the existing PR, not `gh pr create`",
        );
        assert!(
            !queued[0].1.contains("gh pr create"),
            "a worker with a bound PR must never be told to `gh pr create`",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            // Chore stays put — no finalize.
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(t.pr_url.as_deref(), Some(pr_url));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "lease must stay held; the worker has unfinished business"
        );
    }

    #[tokio::test]
    async fn new_pr_flow_still_falls_through_to_cold_detector() {
        // Regression guard: when `chore.pr_url` is empty (new-PR
        // flow, first run of the chore), the SHA-delta gate must
        // declare itself inapplicable and let the existing
        // branch-keyed detector run unchanged. Otherwise the fix
        // would regress the brand-new-PR path.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // No `chore.pr_url` set; no `pr_head_before` snapshot.
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "expected ReviewerEnqueued; got {outcome:?}",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(
            probes.snapshot().is_empty(),
            "new-PR flow must not probe; got {:?}",
            probes.snapshot()
        );
    }

    #[tokio::test]
    async fn resume_with_missing_snapshot_nudges_to_existing_pr_not_create() {
        // Fail-safe: `chore.pr_url` is bound but `pr_head_before` was
        // never captured (e.g. the snapshot fetch failed at run start),
        // so the SHA-delta gate is inapplicable and the cold-path
        // branch detector runs — and misses the PR, because it searches
        // this execution's own branch. Pre-fix that false miss queued
        // `PROBE_NO_PR` ("create a PR"); per T686 the bound PR must be
        // resolved from the structured `pr_url` even with no snapshot,
        // so the worker is pointed at the existing PR instead.
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/606";
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        db.update_work_item(
            &chore_id,
            crate::work::WorkItemPatch {
                pr_url: Some(pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        // Intentionally NOT calling `set_execution_pr_head_before`.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "bound PR exists but worker idle: nudge fires (under the breaker cap)",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].1,
            probe_push_to_existing_pr(pr_url),
            "missing snapshot must NOT regress to `gh pr create`; resolve the bound PR from pr_url",
        );
        assert!(!queued[0].1.contains("gh pr create"));
    }

    // -----------------------------------------------------------
    // Auto-nudge circuit breaker (the Worf incident).
    //
    // exec_18b3945c5b7d7e78_1b (ci_remediation on chore T735) was sent
    // the "produce a PR" nudge 20 times because the chore's PR #869 was
    // bound on a sibling chore_implementation exec, not on the
    // remediation exec's own row — the branch-keyed cold-path search
    // missed it and concluded "no PR". The two guards below pin:
    //   1. A ci_remediation exec whose chore has a bound PR is NEVER
    //      told to `gh pr create`, and the breaker parks it after N
    //      unproductive nudges instead of looping forever.
    //   2. A genuine no-PR chore_implementation exec still gets the
    //      "produce a PR" nudge (healthy case preserved), but the
    //      breaker bounds even that after N.
    // -----------------------------------------------------------

    #[tokio::test]
    async fn ci_remediation_with_bound_pr_never_creates_and_breaker_parks() {
        let workspace = tempdir().unwrap();
        let (db, product_id, _chore_id, execution_id, _attempt_id) =
            ci_remediation_fixture(workspace.path());
        let bound_pr = "https://github.com/spinyfin/mono/pull/88";
        // Cold-path detector finds no PR on the remediation exec's own
        // branch — exactly the Worf false miss.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // Default cap is 3: nudges 1..=3 fire, the 4th trips.
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let mut outcomes = Vec::new();
        for _ in 0..4 {
            outcomes.push(handler.on_stop(&execution_id).await);
        }

        // First three nudges fire; all target the existing PR, none say
        // `gh pr create`, none are the "produce a PR" probe.
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 3, "exactly 3 nudges before the breaker trips");
        for (_, text) in &queued {
            assert_eq!(text, &probe_push_to_existing_pr(bound_pr));
            assert!(!text.contains("gh pr create"), "must never instruct create");
            assert_ne!(text, PROBE_NO_PR, "must never send the produce-a-PR nudge");
        }
        assert!(
            matches!(outcomes[0], StopOutcome::AwaitingInput),
            "first nudge fires; got {:?}",
            outcomes[0]
        );
        assert!(
            matches!(outcomes[3], StopOutcome::NudgeBreakerParked { .. }),
            "the 4th attempt must trip the breaker; got {:?}",
            outcomes[3]
        );

        // The execution is parked with a surfaced attention item.
        let items = db.list_attention_items(&execution_id).unwrap();
        let parked = items
            .iter()
            .find(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND)
            .expect("breaker must file an attention item");
        assert!(
            parked.body_markdown.contains(bound_pr),
            "parked reason should name the existing PR; got {:?}",
            parked.body_markdown
        );
        // Idempotent: only one attention item despite the repeated trips.
        assert_eq!(
            items
                .iter()
                .filter(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND)
                .count(),
            1,
            "repeated trips must not pile up duplicate attention items",
        );
        // Surfaced to the coordinator/UI.
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(p, ev)| p == &product_id
                && matches!(ev, boss_protocol::FrontendEvent::AttentionItemCreated { .. })),
            "an AttentionItemCreated event must be published; got {typed:?}",
        );
    }

    #[tokio::test]
    async fn ci_remediation_with_flaky_retrigger_signal_parks_without_nudging() {
        // Issue #1205: the worker diagnosed the CI failure as flaky/infra
        // and re-ran the job (`mark-retriggered`), which armed the
        // `ci_flaky_retriggered` signal. On the next Stop the completion
        // path must park the worker (no nudge, no diff probe) — the stuck
        // loop is the bug. It must also NOT mark the (already-terminal)
        // attempt failed.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id, attempt_id) =
            ci_remediation_fixture(workspace.path());
        // The worker's marker: flip the attempt terminal + arm the signal.
        db.mark_ci_remediation_retriggered(&attempt_id)
            .unwrap()
            .expect("retrigger flip");

        // Detector finds no PR on the remediation exec's own branch — the
        // same false miss that would otherwise drive the nudge loop.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        // Probe it several times: every Stop must park, never nudge.
        let mut outcomes = Vec::new();
        for _ in 0..3 {
            outcomes.push(handler.on_stop(&execution_id).await);
        }

        assert!(
            probes.snapshot().is_empty(),
            "a flaky-retriggered worker must never be nudged; got {:?}",
            probes.snapshot(),
        );
        for outcome in &outcomes {
            assert!(
                matches!(outcome, StopOutcome::FlakyRetriggered { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/88"),
                "every Stop must park as FlakyRetriggered; got {outcome:?}",
            );
        }

        // The catch-all finalizer must NOT mark the attempt failed — it is
        // terminal `retriggered`, not a give-up.
        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "retriggered");
        assert!(
            attempt.failure_reason.is_none(),
            "retrigger is not a failure; got {:?}",
            attempt.failure_reason,
        );

        // No breaker attention item is filed — parking here is the normal,
        // expected outcome, not a tripped circuit breaker.
        let items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            !items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
            "flaky park must not masquerade as a breaker trip",
        );
        let _ = chore_id;
    }

    #[tokio::test]
    async fn genuine_no_pr_chore_still_nudges_then_breaker_parks() {
        // Healthy case: a chore_implementation exec with no bound PR and
        // no PR on its branch. The legitimate "produce a PR" nudge must
        // still fire — but the breaker bounds it too. Cap lowered to 2
        // to keep the test short.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_max_unproductive_nudges(2);

        let o1 = handler.on_stop(&execution_id).await;
        let o2 = handler.on_stop(&execution_id).await;
        let o3 = handler.on_stop(&execution_id).await;

        let queued = probes.snapshot();
        assert_eq!(queued.len(), 2, "the legitimate produce-a-PR nudge fires up to the cap");
        assert_eq!(queued[0].1, PROBE_NO_PR, "healthy no-PR case must still nudge to create");
        assert_eq!(queued[1].1, PROBE_NO_PR);
        assert!(matches!(o1, StopOutcome::AwaitingInput));
        assert!(matches!(o2, StopOutcome::AwaitingInput));
        assert!(
            matches!(o3, StopOutcome::NudgeBreakerParked { .. }),
            "breaker must bound the no-PR nudge after the cap; got {o3:?}",
        );
        // Chore is untouched (no false finalize); execution stays parked.
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected chore, got {other:?}"),
        }
        let items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
            "parking must file an attention item",
        );
    }

    // -----------------------------------------------------------
    // revision_implementation stop-boundary fix (T-this).
    //
    // A `revision_implementation` execution must NEVER be told to
    // `gh pr create` — the revision's job is to push a new commit to
    // the parent task's EXISTING PR branch.  Two sub-cases pinned:
    //   1. execution.pr_url was not stamped (older exec) but chain root
    //      has a pr_url: chain-root lookup finds the bound PR → worker
    //      gets probe_push_to_existing_pr, never PROBE_NO_PR.
    //   2. No bound PR resolvable at all (anomalous data): park instead
    //      of contradicting the worker with PROBE_NO_PR.
    // -----------------------------------------------------------

    /// Build a revision fixture but leave `execution.pr_url` as NULL
    /// (simulates an execution created before pr_url was reliably stamped).
    /// The parent chore still has `pr_url` set so the chain-root lookup
    /// can find it.
    fn revision_fixture_no_execution_pr_url(
        workspace_path: &Path,
        parent_pr_url: &str,
    ) -> (Arc<WorkDb>, String, String, String) {
        use boss_protocol::CreateRevisionInput;
        use crate::work::{FakePrStateChecker, PrOpenState};

        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss-revision-chain-root-test")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Parent chore")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                rusqlite::params![parent.id, parent_pr_url],
            )
            .unwrap();
        }
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Fix conflict")
                    .build(),
                &checker,
            )
            .unwrap();
        // Create execution WITHOUT pr_url (simulates older dispatch path).
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .prefer_is_soft(true)
                    // Intentionally omitting pr_url to test chain-root fallback.
                    .build(),
            )
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned revision worker pane"),
                None,
                false,
                None,
            )
            .unwrap();
        (db, product.id, revision.id, execution.id)
    }

    #[tokio::test]
    async fn revision_with_null_execution_pr_url_falls_back_to_chain_root_pr() {
        // T-this regression: a `revision_implementation` execution whose
        // `execution.pr_url` is NULL (created before reliable stamping)
        // must not receive PROBE_NO_PR ("open a new PR with `gh pr create`").
        // The chain-root lookup must find the parent chore's pr_url and
        // return `probe_push_to_existing_pr` instead.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
        let (db, _product_id, _revision_id, execution_id) =
            revision_fixture_no_execution_pr_url(workspace.path(), parent_pr_url);
        // Cold-path detector returns None — correct for revisions which
        // have no branch of their own.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "revision with no execution.pr_url must nudge (not PROBE_NO_PR)",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "exactly one nudge queued");
        assert_eq!(
            queued[0].1,
            probe_push_to_existing_pr(parent_pr_url),
            "must use chain-root pr_url, never PROBE_NO_PR",
        );
        assert_ne!(
            queued[0].1, PROBE_NO_PR,
            "revision must NEVER receive the produce-a-PR nudge",
        );
        assert!(
            !queued[0].1.contains("gh pr create"),
            "revision nudge must not mention `gh pr create`",
        );
    }

    #[tokio::test]
    async fn revision_with_no_bound_pr_parks_instead_of_nudging_create() {
        // Safety net: if even the chain-root lookup yields no PR URL
        // (anomalous data — e.g. chain root never opened a PR), the
        // revision execution must be parked rather than nudged with
        // PROBE_NO_PR.  A parked revision surfaces as an attention item
        // for a human to investigate; PROBE_NO_PR would contradict the
        // worker's own task instructions.
        use boss_protocol::CreateRevisionInput;
        use crate::work::{FakePrStateChecker, PrOpenState};

        let workspace = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss-revision-no-pr-test")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        // Parent chore with NO pr_url (never opened a PR).
        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Parent chore no PR")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        // Manually set parent to in_review WITHOUT a pr_url so the
        // revision gate passes (bypassed via direct SQL) but the chain
        // root has no PR to resolve.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![parent.id],
            )
            .unwrap();
            // Force the revision gate to see Open by setting a temporary
            // pr_url, create the revision, then clear it.
            conn.execute(
                "UPDATE tasks SET pr_url = 'https://github.com/spinyfin/mono/pull/999' WHERE id = ?1",
                rusqlite::params![parent.id],
            )
            .unwrap();
        }
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Fix conflict — no PR scenario")
                    .build(),
                &checker,
            )
            .unwrap();
        // Clear the parent pr_url so the chain-root lookup yields None.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET pr_url = NULL WHERE id = ?1",
                rusqlite::params![parent.id],
            )
            .unwrap();
        }
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .prefer_is_soft(true)
                    .build(),
            )
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned revision worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        let outcome = handler.on_stop(&execution.id).await;
        assert!(
            matches!(outcome, StopOutcome::NudgeBreakerParked { .. }),
            "revision with no resolvable bound PR must park, not produce PROBE_NO_PR; got {outcome:?}",
        );
        let queued = probes.snapshot();
        assert!(
            queued.is_empty(),
            "no probe must be queued when parking a revision with no bound PR; got {queued:?}",
        );
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
            "parking must file an attention item",
        );
        // Critical: PROBE_NO_PR must never be queued.
        assert!(
            queued.iter().all(|(_, t)| t != PROBE_NO_PR),
            "revision must never receive PROBE_NO_PR",
        );
    }

    #[tokio::test]
    async fn nudge_breaker_resets_after_worker_finally_opens_pr() {
        // A worker that gets nudged a couple of times and THEN opens a
        // real PR must finalize cleanly — the accumulated nudge count is
        // reset on finalize, so it doesn't carry over to poison a later
        // cycle.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        // First two stops find no PR; the third finds a fresh PR.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        assert!(matches!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput));
        assert!(matches!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput));
        // The worker finally opens a real PR before the breaker trips.
        detector
            .set_result(PrStatus::Fresh {
                url: "https://github.com/foo/bar/pull/77".to_owned(),
            })
            .await;
        let final_outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(final_outcome, StopOutcome::ReviewerEnqueued { .. }),
            "the worker's real PR must finalize; got {final_outcome:?}",
        );
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execution_started_hook_persists_pr_head_before_when_bound() {
        // The run-start hook must snapshot the bound PR's head SHA
        // into `work_executions.pr_head_before` so the Stop-boundary
        // SHA-delta gate has something to compare against. Skips
        // gracefully when no PR is bound (new-PR flow).
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/606";
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        db.update_work_item(
            &chore_id,
            crate::work::WorkItemPatch {
                pr_url: Some(pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let verifier = StubBranchVerifier::ok("boss/exec_old");
        verifier
            .set_head_oid(Ok("abcdef0123456789".into()))
            .await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        // Before the hook: no snapshot.
        assert_eq!(
            db.get_execution(&execution_id).unwrap().pr_head_before,
            None
        );
        handler.on_execution_started(&execution_id).await;
        assert_eq!(
            db.get_execution(&execution_id).unwrap().pr_head_before.as_deref(),
            Some("abcdef0123456789"),
            "hook must persist the snapshot when a PR is bound",
        );
    }

    #[tokio::test]
    async fn execution_started_hook_skips_when_no_pr_bound() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let verifier = StubBranchVerifier::ok("boss/exec_old");
        // A verifier that would explode if called — we expect it not
        // to be touched at all when no PR is bound.
        verifier.set_head_oid(Err("must not be called".into())).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        handler.on_execution_started(&execution_id).await;
        assert_eq!(
            db.get_execution(&execution_id).unwrap().pr_head_before,
            None,
            "no bound PR ⇒ no snapshot",
        );
    }

    // ── Bug B: recheck_for_pr_late ─────────────────────────────────────────

    /// Build a WorkDb with a chore whose execution is `abandoned` and
    /// `workspace_path` is still set (mirrors the double-spawn race where
    /// exec_A is abandoned by the orphan sweep while its pane is running).
    fn abandoned_execution_fixture() -> (Arc<WorkDb>, String, String, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss-late.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Late PR chore".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build())
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/workspaces/mono-agent-001",
            )
            .unwrap();
        // Mirror the waiting_human state.
        db.finish_execution_run(
            &execution.id,
            &run.id,
            ExecutionStatus::WaitingHuman,
            "completed",
            Some("spawned pane"),
            None,
            false,
            None,
        )
        .unwrap();
        // Simulate orphan sweep abandoning exec_A.
        db.mark_execution_redundant(&execution.id).unwrap();
        (db, product.id, chore.id, execution.id)
    }

    #[tokio::test]
    async fn recheck_for_pr_late_binds_pr_to_active_task() {
        let (db, _product_id, chore_id, execution_id) = abandoned_execution_fixture();
        let detector =
            StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(db.clone(), detector, cube, publisher, pane, probes);

        let candidate = crate::work::LatePrCandidate {
            execution_id: execution_id.clone(),
            work_item_id: chore_id.clone(),
            repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
            branch_naming: BranchNaming::BossExecPrefix,
            worker_branch_prefix: None,
        };
        let outcome = handler.recheck_for_pr_late(&candidate).await;

        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "expected PrDetected, got {outcome:?}"
        );
        let task = match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, TaskStatus::InReview);
        assert_eq!(
            task.pr_url.as_deref(),
            Some("https://github.com/spinyfin/mono/pull/42")
        );
        // Execution itself stays abandoned — recheck_for_pr_late does not
        // touch the execution row.
        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Abandoned);
    }

    #[tokio::test]
    async fn recheck_for_pr_late_returns_awaiting_input_when_no_pr() {
        let (db, _product_id, chore_id, execution_id) = abandoned_execution_fixture();
        let detector = StubPrDetector::ok(None); // no PR found
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(db.clone(), detector, cube, publisher, pane, probes);

        let candidate = crate::work::LatePrCandidate {
            execution_id: execution_id.clone(),
            work_item_id: chore_id.clone(),
            repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
            branch_naming: BranchNaming::BossExecPrefix,
            worker_branch_prefix: None,
        };
        let outcome = handler.recheck_for_pr_late(&candidate).await;

        assert!(
            matches!(outcome, StopOutcome::AwaitingInput),
            "expected AwaitingInput when no PR found, got {outcome:?}"
        );
        // Chore stays active.
        let task = match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, TaskStatus::Active);
        assert!(task.pr_url.is_none());
    }

    // -----------------------------------------------------------
    // Revision task completion via SHA-delta gate in recheck_for_pr
    //
    // Reproduces T848: a revision worker pushed its commit to the parent
    // PR but the revision task stayed in `doing` (active). The on_stop SHA
    // delta gate failed transiently; the merge-poller's recheck_for_pr had
    // no SHA-delta fallback, so the revision was stranded forever.
    //
    // The fix adds the SHA-delta gate to recheck_for_pr. Tests below pin:
    //   1. Revision worker pushed → SHA moved → recheck_for_pr finalises.
    //   2. Revision worker not yet pushed → SHA unchanged → recheck quiet.
    //   3. Revision with no pr_head_before snapshot → Inapplicable → cold
    //      path still runs (returns quiet; no regression).
    // -----------------------------------------------------------

    /// Build a fixture simulating a revision task whose worker has been
    /// spawned and is in `waiting_human` state. The parent chore carries
    /// `parent_pr_url` and the revision execution's `pr_url` is set to
    /// the same URL (as the dispatcher does at create time). `head_before`
    /// is stored as `pr_head_before` to simulate the snapshot taken by
    /// `on_execution_started`.
    fn revision_fixture(
        workspace_path: &Path,
        parent_pr_url: &str,
        head_before: &str,
    ) -> (Arc<WorkDb>, String, String, String) {
        use boss_protocol::CreateRevisionInput;
        use crate::work::{FakePrStateChecker, PrOpenState};

        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss-revision-test")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        // Parent chore: in_review with a bound pr_url.
        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Parent chore")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                rusqlite::params![parent.id, parent_pr_url],
            )
            .unwrap();
        }
        // Revision task: created against the parent.
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Add missing builder derive")
                    .build(),
                &checker,
            )
            .unwrap();
        // Execution: revision_implementation with pr_url = parent PR URL.
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .prefer_is_soft(true)
                    .pr_url(parent_pr_url)
                    .build(),
            )
            .unwrap();
        // Mirror PaneSpawnRunner: start → running (task → active), then
        // finish → waiting_human (pane spawned, engine waiting for Claude).
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned revision worker pane"),
                None,
                false,
                None,
            )
            .unwrap();
        // Snapshot the parent PR's head SHA as `on_execution_started` does.
        db.set_execution_pr_head_before(&execution.id, head_before)
            .unwrap();
        (db, product.id, revision.id, execution.id)
    }

    #[tokio::test]
    async fn recheck_for_pr_sha_delta_advances_revision_to_in_review() {
        // T848 regression: revision worker pushed a commit to the parent
        // PR (head SHA changed), but `on_stop` failed to detect it (GitHub
        // API timeout during SHA fetch). The merge-poller's `recheck_for_pr`
        // should advance the revision to `in_review` on the next sweep via
        // the SHA-delta gate.
        //
        // Before the fix, `recheck_for_pr` had no SHA-delta gate; it fell
        // through to the cold-path branch-keyed detector which always returns
        // None for revisions (they never open their own PR), so the revision
        // stayed in `active` indefinitely.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/922";
        let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head_before);
        // Cold-path detector returns None — correct for revisions which
        // have no branch of their own.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        // Branch verifier: SHA moved (worker pushed the revision commit).
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier
            .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
            .await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.recheck_for_pr(&execution_id).await;

        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
            "SHA-delta gate must advance revision to in_review when head moved; got {outcome:?}",
        );
        // Revision task must be in_review; pr_url stays NULL (revisions don't own PRs).
        let item = db.get_work_item(&revision_id).unwrap();
        match item {
            WorkItem::Task(t) => {
                assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
                assert!(
                    t.pr_url.is_none(),
                    "revision pr_url must stay NULL; parent owns the PR"
                );
            }
            other => panic!("expected task, got {other:?}"),
        }
        // Execution must be completed; lease must be released.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.finished_at.is_some());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "cube lease must be released after revision finalises",
        );
        // Work-item changed event must fire so the kanban updates.
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events.iter().any(|(p, w, _)| p == &product_id && w == &revision_id),
            "work-item invalidation must fire for the revision, got {work_events:?}",
        );
        // No probe must be queued — the revision is done.
        assert!(
            probes.snapshot().is_empty(),
            "no probe must fire when revision is finalised; got {:?}",
            probes.snapshot(),
        );
    }

    #[tokio::test]
    async fn recheck_for_pr_sha_unchanged_leaves_revision_active() {
        // Revision worker has not pushed yet (no commit since execution
        // started). The SHA-delta gate returns NoContribution; the cold
        // path returns quietly (no PR on revision branch). The revision
        // stays in `active` so the merge-poller will retry on the next
        // sweep.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/922";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        // Branch verifier: SHA unchanged.
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.recheck_for_pr(&execution_id).await;

        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "unchanged SHA means revision not yet done; got {outcome:?}",
        );
        let item = db.get_work_item(&revision_id).unwrap();
        match item {
            WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected task, got {other:?}"),
        }
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "lease must stay held when revision is not done",
        );
        assert!(probes.snapshot().is_empty(), "recheck must not nudge");
    }

    // -----------------------------------------------------------
    // PR-metadata-only CI-fix revision finalize (issue #1252), re-solved
    // without the #1262 regression (rolled back in #1293).
    //
    // A CI-fix revision can legitimately finish WITHOUT moving the bound
    // PR head — it repairs a PR-description validator via `gh pr edit
    // --body`, no commit. The SHA-delta gate returns NoContribution on
    // every sweep. We finalize such a revision ONLY on positive evidence:
    //   1. a real Stop boundary (only `on_stop` stamps the marker; a
    //      dead/cut-off worker emits no Stop hook), AND
    //   2. an operator-visible PR-body delta (live body != run-start
    //      snapshot), AND
    //   3. CI green on the bound PR.
    // The merge poller may finalize only what `on_stop` already marked,
    // so a worker that contributed nothing (R1 dead, R2 reaped-while-live)
    // is never mis-finalized.
    // -----------------------------------------------------------

    /// Configurable [`MergeProbe`] returning a fixed lifecycle state for
    /// any PR url. Drives the bound-PR-health check in the metadata-fix
    /// finalize path.
    struct FixedStateProbe(crate::merge_poller::PrLifecycleState);
    #[async_trait]
    impl MergeProbe for FixedStateProbe {
        async fn probe(
            &self,
            _pr_url: &str,
        ) -> anyhow::Result<crate::merge_poller::PrLifecycleProbe> {
            Ok(crate::merge_poller::PrLifecycleProbe {
                url: String::new(),
                state: self.0.clone(),
                base_ref_oid: None,
                head_ref_oid: None,
                head_ref_name: None,
                base_ref_name: None,
                labels: Vec::new(),
                review: crate::merge_poller::PrReviewState::Unknown,
                in_merge_queue: false,
            })
        }
    }

    #[tokio::test]
    async fn on_stop_finalizes_metadata_only_revision_when_body_changed_and_ci_clean() {
        use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1252";
        let head = "1111111111111111111111111111111111111111";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        // Worker edited the PR body during this run: live body differs from
        // the run-start snapshot. Head SHA unchanged → NoContribution.
        db.set_execution_pr_body_before(&execution_id, "## Summary\nold body")
            .unwrap();
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;
        verifier
            .set_body(Ok("## Summary\nold body\n\n## Testing\nfixed PR-template check".into()))
            .await;
        // The PR-template check went green after the edit.
        let probe: Arc<dyn MergeProbe> =
            Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(probe);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
            "metadata-only CI-fix revision with a body delta + clean CI must finalize to \
             in_review; got {outcome:?}",
        );
        // Positive-evidence marker stamped.
        assert!(
            db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
            "on_stop must stamp the metadata-fix marker after observing the body delta",
        );
        // Revision advanced out of Doing to Review (revisions never own a pr_url).
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::InReview);
                assert!(t.pr_url.is_none(), "revision tasks must not own a pr_url");
            }
            other => panic!("expected revision task, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.cube_lease_id.is_none());
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert!(
            probes.snapshot().is_empty(),
            "a clean metadata-only completion must NOT nudge",
        );
    }

    #[tokio::test]
    async fn on_stop_records_marker_but_awaits_ci_when_body_changed_but_ci_not_green() {
        use crate::merge_poller::{OpenPrStatus, PrLifecycleState, RequiredCheckFailure};

        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1253";
        let head = "2222222222222222222222222222222222222222";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        db.set_execution_pr_body_before(&execution_id, "old body")
            .unwrap();
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;
        verifier.set_body(Ok("edited body that fixes the template".into())).await;
        // The PR-template check is still re-running after the edit.
        let failures = vec![RequiredCheckFailure {
            name: "pr-template".into(),
            conclusion: "IN_PROGRESS".into(),
            target_url: String::new(),
            provider: crate::merge_poller::CiProvider::GithubActions,
            provider_job_id: None,
        }];
        let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(
            OpenPrStatus::ci_failing(failures),
        )));

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(probe);

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "body delta + not-yet-green CI must record the marker and await CI, not nudge; \
             got {outcome:?}",
        );
        // Marker persisted so the merge poller can finalize once CI greens.
        assert!(
            db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
            "the metadata-fix marker must persist for the poller's later finalize",
        );
        // Revision stays in Doing; lease held; no nudge (nothing to push).
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
            other => panic!("expected task, got {other:?}"),
        }
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::WaitingHuman);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        assert!(
            probes.snapshot().is_empty(),
            "a recorded metadata-only fix awaiting CI must NOT nudge the worker to push",
        );
    }

    #[tokio::test]
    async fn on_stop_does_not_finalize_revision_when_body_unchanged() {
        // R2 at the Stop boundary: the worker made no commit AND no PR-body
        // edit (head unchanged, body unchanged). This is "contributed
        // nothing", NOT a clean no-op completion. It must never be marked
        // or finalized as a metadata-only fix — it falls through to the
        // normal nudge.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1254";
        let head = "3333333333333333333333333333333333333333";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        db.set_execution_pr_body_before(&execution_id, "unchanged body")
            .unwrap();
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;
        verifier.set_body(Ok("unchanged body".into())).await; // identical → no delta

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let _ = handler.on_stop(&execution_id).await;
        assert!(
            !db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
            "no body delta must NOT stamp the metadata-fix marker",
        );
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
                t.status, TaskStatus::Active,
                "a no-contribution run must not be finalized as a metadata-only fix",
            ),
            other => panic!("expected task, got {other:?}"),
        }
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::WaitingHuman);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recheck_finalizes_metadata_only_revision_after_ci_greens_when_marked() {
        // The CI-went-green-after-Stop recovery: on_stop already stamped the
        // marker (real Stop boundary + body delta) but CI was still
        // re-running. A later merge-poller sweep finalizes it now CI is green.
        use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1255";
        let head = "4444444444444444444444444444444444444444";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        // Simulate the marker on_stop stamped on a prior turn.
        db.mark_execution_metadata_fix_confirmed(&execution_id).unwrap();
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await; // head unchanged → NoContribution
        let probe: Arc<dyn MergeProbe> =
            Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(probe);

        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
            "marked metadata-only revision must finalize once its bound PR CI is green; \
             got {outcome:?}",
        );
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
            other => panic!("expected task, got {other:?}"),
        }
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Completed);
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert!(probes.snapshot().is_empty(), "recovery must not nudge");
    }

    #[tokio::test]
    async fn recheck_does_not_finalize_unmarked_revision_even_with_green_ci() {
        // The #1262 regression guard (T1256 R1 dead worker, T1265 R2 live
        // worker). The bound PR head is unchanged and CI is GREEN, but
        // on_stop never stamped the marker (the worker died / was reaped
        // before reaching a clean Stop with an operator-visible delta). The
        // merge poller must NOT finalize it — that was the rolled-back
        // behaviour. It stays Doing for the incomplete-execution paths.
        use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1256";
        let head = "5555555555555555555555555555555555555555";
        let (db, _product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head);
        // NO marker stamped (the load-bearing difference from the test above).
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await; // head unchanged → NoContribution
        // CI is green — proving we gate on the marker, not on "head
        // unchanged + CI green".
        let probe: Arc<dyn MergeProbe> =
            Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(probe);

        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "an unmarked revision must NOT finalize even with green CI; got {outcome:?}",
        );
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
                t.status, TaskStatus::Active,
                "the #1262 regression must stay fixed: no marker means no finalize",
            ),
            other => panic!("expected task, got {other:?}"),
        }
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::WaitingHuman);
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "an unmarked revision's lease must stay held (not reaped)",
        );
        assert!(pane.calls.lock().await.is_empty());
        assert!(probes.snapshot().is_empty());
    }

    // -----------------------------------------------------------
    // T939 regression: revision on_stop with pr_head_before set
    //
    // When on_stop fires for a revision_implementation execution in
    // waiting_human status with pr_head_before captured at execution start:
    //   1. SHA-delta Contributed (worker pushed) → finalize directly, no nudge.
    //   2. SHA-delta Inapplicable due to transient API failure → return quietly,
    //      no nudge (avoids the probe loop: probe → response → Stop → nudge →
    //      repeat that kept Crusher stuck in T939).
    // The merge poller's recheck_for_pr handles case 2 when the API recovers.
    // -----------------------------------------------------------

    #[tokio::test]
    async fn revision_on_stop_sha_delta_contributed_finalizes_with_no_nudge() {
        // T939 ideal path: the revision worker pushed commits to the parent PR
        // branch (head SHA moved). on_stop detects the contribution via the
        // SHA-delta gate and finalizes without queuing any nudge probe.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1032";
        let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, product_id, revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head_before);
        // Branch verifier: SHA moved (worker pushed revision commit).
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier
            .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
            .await;
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
            "on_stop must finalize revision when SHA-delta detects contribution; got {outcome:?}",
        );
        // No probe must be queued — the revision is done.
        assert!(
            probes.snapshot().is_empty(),
            "no probe must fire when revision is finalised via SHA-delta; got {:?}",
            probes.snapshot(),
        );
        // Revision task must be in_review; task.pr_url stays NULL (parent owns it).
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) => {
                assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
                assert!(t.pr_url.is_none(), "revision task.pr_url must stay NULL");
            }
            other => panic!("expected task, got {other:?}"),
        }
        // Execution must be completed with pr_url populated (= parent PR URL).
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert_eq!(
            execution.pr_url.as_deref(),
            Some(parent_pr_url),
            "execution.pr_url must be populated with parent PR URL after finalization",
        );
        // Cube lease must be released.
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "cube lease must be released after revision finalises",
        );
        // Work-item invalidation must fire.
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events.iter().any(|(p, w, _)| p == &product_id && w == &revision_id),
            "work-item invalidation must fire for the revision, got {work_events:?}",
        );
    }

    #[tokio::test]
    async fn revision_on_stop_sha_delta_api_failure_does_not_nudge() {
        // T939 regression fix: when on_stop fires for a revision_implementation
        // execution in waiting_human with pr_head_before set, but the GitHub
        // API fails transiently (SHA-delta gate → Inapplicable), the engine
        // must NOT queue a nudge probe. Queuing a probe causes the worker to
        // respond, which fires another Stop, which nudges again — an infinite
        // loop. Return AwaitingInput silently; the merge poller's recheck_for_pr
        // will finalize once the API recovers.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/1032";
        let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, _product_id, _revision_id, execution_id) =
            revision_fixture(workspace.path(), parent_pr_url, head_before);
        // Branch verifier: fetch_pr_head_oid fails — simulates transient GitHub API failure.
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier
            .set_head_oid(Err("transient GitHub API error".to_owned()))
            .await;
        // Cold-path detector returns None — revision has no branch of its own.
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(
            outcome,
            StopOutcome::AwaitingInput,
            "revision with pr_head_before set but transient SHA-delta failure must return \
             AwaitingInput silently (no nudge loop); got {outcome:?}",
        );
        // CRITICAL: no probe must be queued.
        assert!(
            probes.snapshot().is_empty(),
            "revision must NOT be nudged when SHA-delta fails with pr_head_before set \
             (T939 regression guard); got {:?}",
            probes.snapshot(),
        );
        // Execution must still be waiting_human — not completed, not parked.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.status, ExecutionStatus::WaitingHuman,
            "execution must remain in waiting_human until merge poller finalizes it",
        );
    }

    // -----------------------------------------------------------
    // Signal-already-cleared gate tests
    //
    // The gate fires in the NoContribution arm: conflict/CI revision worker
    // stops without pushing, but the blocking signal is already cleared.
    // Expected: attempt retired as succeeded, parent snapped to in_review,
    // execution finalised, NO nudge.
    // -----------------------------------------------------------

    /// Build a conflict-resolution revision fixture. The parent chore is
    /// `blocked: merge_conflict`. A `conflict_resolutions` row is inserted
    /// in `running` state (simulating an active attempt). A revision task is
    /// created for the fix; a `revision_implementation` execution is left in
    /// `waiting_human` with `pr_head_before = head_before` (the SHA-delta
    /// snapshot). `created_via` is set to `"merge-conflict:<attempt_id>"`.
    ///
    /// Returns `(db, product_id, parent_chore_id, revision_id, execution_id, attempt_id, pr_url)`.
    #[allow(clippy::too_many_arguments)]
    fn conflict_revision_fixture(
        workspace_path: &Path,
        parent_pr_url: &str,
        head_before: &str,
    ) -> (Arc<WorkDb>, String, String, String, String, String) {
        use boss_protocol::CreateRevisionInput;
        use crate::work::{ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState};

        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss-conflict-rev-test")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        // Parent chore: blocked:merge_conflict with a bound pr_url.
        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Parent chore")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict', \
                 pr_url = ?2 WHERE id = ?1",
                rusqlite::params![parent.id, parent_pr_url],
            )
            .unwrap();
        }
        // Insert a conflict_resolutions attempt (Phase 3 style: `pending`,
        // no cube_lease_id — the fix vehicle is a revision_implementation
        // execution, not a bespoke conflict_resolution execution).
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: parent.id.clone(),
                pr_url: parent_pr_url.to_owned(),
                pr_number: 966,
                head_branch: "my-feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base_sha_1".into()),
                head_sha_before: Some(head_before.into()),
            })
            .unwrap()
            .unwrap();
        // Revision task with created_via = "merge-conflict:<attempt_id>".
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Resolve merge conflict against main")
                    .created_via(format!("merge-conflict:{}", attempt.id))
                    .build(),
                &checker,
            )
            .unwrap();
        // Stamp the reverse link (as conflict_watch::on_conflict_detected does).
        db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        // Execution: revision_implementation with pr_url = parent PR URL.
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .prefer_is_soft(true)
                    .pr_url(parent_pr_url)
                    .build(),
            )
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-033",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned conflict-resolution worker pane"),
                None,
                false,
                None,
            )
            .unwrap();
        db.set_execution_pr_head_before(&execution.id, head_before)
            .unwrap();
        (
            db,
            product.id,
            parent.id,
            revision.id,
            execution.id,
            attempt.id,
        )
    }

    #[tokio::test]
    async fn conflict_revision_signal_cleared_retires_attempt_and_finalises() {
        // Riker scenario (T927 / exec_18b431dc9b016e88_1a regression):
        // conflict-resolution revision worker stops without pushing because
        // the conflict was already resolved by a sibling. The SHA-delta gate
        // returns NoContribution; the signal-cleared gate must detect the PR
        // is now mergeable, retire the attempt as succeeded, snap the parent
        // back to in_review, and finalise the execution — no nudge.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
            conflict_revision_fixture(workspace.path(), parent_pr_url, head);

        let detector = StubPrDetector::ok(None); // no branch-keyed PR
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // SHA-delta gate: head unchanged → NoContribution.
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        // MergeProbe: PR is now mergeable (conflict cleared).
        struct CleanMergeProbe;
        #[async_trait]
        impl MergeProbe for CleanMergeProbe {
            async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
                Ok(PrLifecycleProbe {
                    url: url.to_owned(),
                    state: PrLifecycleState::Open(crate::merge_poller::OpenPrStatus::clean()),
                    base_ref_oid: None,
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: crate::merge_poller::PrReviewState::Unknown,
                    in_merge_queue: false,
                })
            }
        }

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(CleanMergeProbe));

        let outcome = handler.on_stop(&execution_id).await;

        assert!(
            matches!(outcome, StopOutcome::SignalAlreadyCleared { ref pr_url } if pr_url == parent_pr_url),
            "signal-cleared gate must short-circuit the nudge; got {outcome:?}",
        );

        // Conflict attempt must be succeeded.
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "succeeded",
            "conflict_resolutions attempt must be retired as succeeded",
        );

        // Parent chore must be snapped back to in_review.
        let parent = match db.get_work_item(&parent_chore_id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            parent.status, TaskStatus::InReview,
            "parent chore must be snapped back to in_review",
        );

        // Execution must be finalised (completed, lease released).
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "execution cube lease must be released",
        );
        assert!(
            !pane.calls.lock().await.is_empty(),
            "pane must be released after finalisation",
        );

        // No probe must be queued — the worker is done.
        assert!(
            probes.snapshot().is_empty(),
            "no nudge probe must fire on signal-cleared path; got {:?}",
            probes.snapshot(),
        );

        // ConflictResolutionSucceeded frontend event must have been published.
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(pid, ev)| {
                pid == &product_id
                    && matches!(
                        ev,
                        FrontendEvent::ConflictResolutionSucceeded {
                            work_item_id,
                            ..
                        } if work_item_id == &parent_chore_id
                    )
            }),
            "ConflictResolutionSucceeded must be published; typed events: {typed:?}",
        );
    }

    #[tokio::test]
    async fn conflict_revision_signal_still_active_nudges_as_before() {
        // Regression guard: if the conflict is STILL active when the worker
        // stops without pushing, the normal nudge path must still fire —
        // the signal-cleared gate must not suppress it.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
            conflict_revision_fixture(workspace.path(), parent_pr_url, head);

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // SHA-delta gate: head unchanged → NoContribution.
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        // MergeProbe: PR is STILL conflicting.
        struct ConflictingMergeProbe;
        #[async_trait]
        impl MergeProbe for ConflictingMergeProbe {
            async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
                Ok(PrLifecycleProbe {
                    url: url.to_owned(),
                    state: PrLifecycleState::Open(
                        crate::merge_poller::OpenPrStatus::conflict_only(),
                    ),
                    base_ref_oid: None,
                    head_ref_oid: None,
                    head_ref_name: None,
                    base_ref_name: None,
                    labels: Vec::new(),
                    review: crate::merge_poller::PrReviewState::Unknown,
                    in_merge_queue: false,
                })
            }
        }

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(ConflictingMergeProbe));

        let outcome = handler.on_stop(&execution_id).await;

        // Signal still active → normal nudge, NOT SignalAlreadyCleared.
        assert!(
            matches!(outcome, StopOutcome::AwaitingInput),
            "signal still active must fall through to normal nudge; got {outcome:?}",
        );

        // Conflict attempt must NOT be retired (still pending — Phase 3 style).
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_ne!(
            attempt.status, "succeeded",
            "conflict attempt must NOT be retired when signal is still active",
        );

        // Probe must have been queued (the nudge fired).
        let queued = probes.snapshot();
        assert_eq!(
            queued.len(),
            1,
            "exactly one nudge probe must be queued; got {queued:?}",
        );
    }

    /// Build a CI-remediation revision fixture. The parent chore is
    /// `blocked: ci_failure` with a bound `pr_url`. A `ci_remediations` row
    /// is inserted (`running`) carrying `failed_checks` (the JSON list of
    /// checks this attempt was opened to fix). A revision task is created
    /// for the fix (`created_via = "ci-fix:<attempt_id>"`) and its id stamped
    /// back onto the attempt. A `revision_implementation` execution is left
    /// in `waiting_human` with `pr_head_before = head` (the SHA-delta
    /// snapshot, so the gate returns NoContribution on an unmoved head).
    ///
    /// Returns `(db, product_id, parent_chore_id, revision_id, execution_id, attempt_id)`.
    fn ci_revision_fixture(
        workspace_path: &Path,
        parent_pr_url: &str,
        head: &str,
        failed_checks: &str,
    ) -> (Arc<WorkDb>, String, String, String, String, String) {
        use boss_protocol::CreateRevisionInput;
        use crate::work::{FakePrStateChecker, PrOpenState};

        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss-ci-rev-test")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Fix failing CI: Pull Request Description")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &parent.id,
            crate::work::WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(parent_pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        let attempt = db
            .insert_ci_remediation(crate::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: parent.id.clone(),
                pr_url: parent_pr_url.into(),
                pr_number: 440,
                head_branch: "my-feature".into(),
                head_sha_at_trigger: head.into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: failed_checks.into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.mark_chore_blocked_ci_failure(&parent.id, parent_pr_url, Some(&attempt.id))
            .unwrap();
        db.mark_ci_remediation_running(&attempt.id, "lease-1", "ws-1", "worker-1")
            .unwrap();

        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent.id.clone())
                    .description("Add the required PR description sections")
                    .created_via(format!("ci-fix:{}", attempt.id))
                    .build(),
                &checker,
            )
            .unwrap();
        db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id)
            .unwrap();

        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .prefer_is_soft(true)
                    .pr_url(parent_pr_url)
                    .build(),
            )
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("spawned CI-fix revision worker pane"),
                None,
                false,
                None,
            )
            .unwrap();
        db.set_execution_pr_head_before(&execution.id, head)
            .unwrap();
        (
            db,
            product.id,
            parent.id,
            revision.id,
            execution.id,
            attempt.id,
        )
    }

    /// Build a `PrLifecycleProbe` for an open PR with the given CI status.
    fn ci_probe(ci: crate::merge_poller::OpenPrCiStatus) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: String::new(),
            state: PrLifecycleState::Open(crate::merge_poller::OpenPrStatus {
                mergeability: crate::merge_poller::OpenPrMergeability::Clean,
                ci,
            }),
            base_ref_oid: None,
            head_ref_oid: None,
            head_ref_name: None,
            base_ref_name: None,
            labels: Vec::new(),
            review: crate::merge_poller::PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    fn failing_check(name: &str) -> crate::merge_poller::RequiredCheckFailure {
        crate::merge_poller::RequiredCheckFailure {
            name: name.to_owned(),
            conclusion: "FAILURE".to_owned(),
            target_url: String::new(),
            provider: crate::merge_poller::CiProvider::Other,
            provider_job_id: None,
        }
    }

    #[tokio::test]
    async fn ci_revision_target_check_cleared_retires_despite_other_failing() {
        // T57 / linkedin-multiproduct/rdev-base-image#440 regression: a
        // CI-remediation revision worker fixed the "Pull Request Description"
        // check via a metadata-only `gh pr edit` (NO commit → SHA-delta gate
        // returns NoContribution). The target check is now green, but the PR
        // has an UNRELATED failing required check. The old heuristic required
        // whole-PR `Clean`, so it re-nudged the worker forever. The fix: the
        // attempt's own targeted check is no longer failing, so it must be
        // retired as succeeded and the parent snapped back to in_review.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
        let (db, product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
            ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        // The target check ("Pull Request Description") is green now; only an
        // unrelated check ("build") is failing.
        struct OtherFailingProbe;
        #[async_trait]
        impl MergeProbe for OtherFailingProbe {
            async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
                let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::Failing {
                    failures: vec![failing_check("build")],
                });
                p.url = url.to_owned();
                Ok(p)
            }
        }

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(OtherFailingProbe));

        let outcome = handler.on_stop(&execution_id).await;

        assert!(
            matches!(outcome, StopOutcome::SignalAlreadyCleared { ref pr_url } if pr_url == parent_pr_url),
            "targeted check cleared must retire the attempt; got {outcome:?}",
        );

        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "succeeded",
            "ci_remediations attempt must be retired as succeeded",
        );

        let parent = match db.get_work_item(&parent_chore_id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            parent.status, TaskStatus::InReview,
            "parent chore must be snapped back to in_review",
        );

        assert!(
            probes.snapshot().is_empty(),
            "no nudge probe must fire when the targeted check is cleared; got {:?}",
            probes.snapshot(),
        );

        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(pid, ev)| {
                pid == &product_id
                    && matches!(
                        ev,
                        FrontendEvent::CiRemediationSucceeded { work_item_id, .. }
                            if work_item_id == &parent_chore_id
                    )
            }),
            "CiRemediationSucceeded must be published; typed events: {typed:?}",
        );
    }

    #[tokio::test]
    async fn ci_revision_target_check_still_failing_nudges_as_before() {
        // Regression guard: when the attempt's OWN targeted check is still
        // failing, the signal is NOT cleared — the normal nudge path must
        // still fire and the attempt must remain active.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
        let (db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
            ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        // The target check is STILL failing.
        struct TargetFailingProbe;
        #[async_trait]
        impl MergeProbe for TargetFailingProbe {
            async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
                let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::Failing {
                    failures: vec![failing_check("Pull Request Description")],
                });
                p.url = url.to_owned();
                Ok(p)
            }
        }

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(TargetFailingProbe));

        let outcome = handler.on_stop(&execution_id).await;

        assert!(
            matches!(outcome, StopOutcome::AwaitingInput),
            "target check still failing must fall through to the normal nudge; got {outcome:?}",
        );

        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_ne!(
            attempt.status, "succeeded",
            "attempt must NOT be retired while its targeted check is still failing",
        );

        assert_eq!(
            probes.snapshot().len(),
            1,
            "exactly one nudge probe must be queued; got {:?}",
            probes.snapshot(),
        );
    }

    #[tokio::test]
    async fn ci_revision_target_check_inflight_does_not_retire() {
        // When CI is InFlight (some required check still non-terminal) we
        // cannot tell whether the targeted check specifically went green, so
        // we must stay conservative and NOT retire — the next sweep
        // re-evaluates once checks terminalize.
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
        let (db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
            ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;

        struct InFlightProbe;
        #[async_trait]
        impl MergeProbe for InFlightProbe {
            async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
                let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::InFlight);
                p.url = url.to_owned();
                Ok(p)
            }
        }

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(InFlightProbe));

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::AwaitingInput),
            "InFlight CI must not retire; got {outcome:?}",
        );
        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_ne!(attempt.status, "succeeded");
    }

    // ── ci_attempt_signal_cleared: predicate decision-table tests ────────────

    #[test]
    fn ci_signal_cleared_clean_always_clears() {
        // Clean clears any attempt, including one with no targeted-check info.
        assert!(ci_attempt_signal_cleared("[]", &OpenPrCiStatus::Clean));
        assert!(ci_attempt_signal_cleared(
            r#"[{"name":"x"}]"#,
            &OpenPrCiStatus::Clean
        ));
    }

    #[test]
    fn ci_signal_cleared_inflight_never_clears() {
        assert!(!ci_attempt_signal_cleared("[]", &OpenPrCiStatus::InFlight));
        assert!(!ci_attempt_signal_cleared(
            r#"[{"name":"x"}]"#,
            &OpenPrCiStatus::InFlight
        ));
    }

    #[test]
    fn ci_signal_cleared_failing_clears_when_target_not_among_failures() {
        // Targeted "Pull Request Description" is green; only "build" fails.
        let ci = OpenPrCiStatus::Failing {
            failures: vec![failing_check("build")],
        };
        assert!(ci_attempt_signal_cleared(
            r#"[{"name":"Pull Request Description"}]"#,
            &ci
        ));
    }

    #[test]
    fn ci_signal_cleared_failing_does_not_clear_when_target_still_failing() {
        let ci = OpenPrCiStatus::Failing {
            failures: vec![failing_check("Pull Request Description"), failing_check("build")],
        };
        assert!(!ci_attempt_signal_cleared(
            r#"[{"name":"Pull Request Description"}]"#,
            &ci
        ));
    }

    #[test]
    fn ci_signal_cleared_failing_with_no_targeted_names_stays_conservative() {
        // No parseable targeted names → only Clean would clear; a Failing
        // status must not retire (preserves pre-change behaviour).
        let ci = OpenPrCiStatus::Failing {
            failures: vec![failing_check("build")],
        };
        assert!(!ci_attempt_signal_cleared("[]", &ci));
        assert!(!ci_attempt_signal_cleared("not json", &ci));
    }

    #[test]
    fn ci_signal_cleared_multi_target_requires_all_targets_clear() {
        // An attempt targeting two checks clears only when BOTH are green.
        let targets = r#"[{"name":"a"},{"name":"b"}]"#;
        let one_left = OpenPrCiStatus::Failing {
            failures: vec![failing_check("b")],
        };
        assert!(!ci_attempt_signal_cleared(targets, &one_left));
        let unrelated = OpenPrCiStatus::Failing {
            failures: vec![failing_check("c")],
        };
        assert!(ci_attempt_signal_cleared(targets, &unrelated));
    }

    #[test]
    fn targeted_check_names_parses_names() {
        assert_eq!(
            targeted_check_names(r#"[{"name":"a"},{"name":"b"}]"#),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(targeted_check_names("[]").is_empty());
        assert!(targeted_check_names("garbage").is_empty());
        assert!(targeted_check_names(r#"{"name":"a"}"#).is_empty());
    }

    // ── expected_branch_name: BranchNaming strategy tests ────────────────────

    #[test]
    fn boss_exec_prefix_produces_classic_branch_name() {
        let exec_id = "exec_18b44d2630b1df80_66";
        let branch = expected_branch_name(exec_id, &BranchNaming::BossExecPrefix, None);
        assert_eq!(branch, "boss/exec_18b44d2630b1df80_66");
        assert!(branch.contains(exec_id), "BossExecPrefix must embed the full execution id");
    }

    #[test]
    fn boss_exec_prefix_honors_product_worker_branch_prefix() {
        // Regression for #1141: a product configured with
        // `worker_branch_prefix = "bduff/"` must produce
        // `bduff/exec_<id>`, not the hardcoded `boss/exec_<id>`. The
        // prefix carries its own trailing `/` and is concatenated
        // verbatim, and the full execution id is preserved.
        let exec_id = "exec_18b44d2630b1df80_66";
        let branch =
            expected_branch_name(exec_id, &BranchNaming::BossExecPrefix, Some("bduff/"));
        assert_eq!(branch, "bduff/exec_18b44d2630b1df80_66");
    }

    #[test]
    fn non_default_branch_naming_takes_precedence_over_worker_branch_prefix() {
        // A non-default editorial `branch_naming` is the richer, explicit
        // rule and wins over the plain `worker_branch_prefix` column, which
        // only shapes the default `BossExecPrefix` strategy.
        let exec_id = "exec_18b44d2630b1df80_66";
        let opaque =
            expected_branch_name(exec_id, &BranchNaming::OpaqueHash, Some("bduff/"));
        assert!(opaque.starts_with("boss/"), "OpaqueHash ignores worker_branch_prefix");
        let custom = expected_branch_name(
            exec_id,
            &BranchNaming::CustomPrefix { prefix: "lnkd".to_owned() },
            Some("bduff/"),
        );
        assert!(custom.starts_with("lnkd/"), "CustomPrefix ignores worker_branch_prefix");
    }

    #[test]
    fn opaque_hash_produces_8_hex_char_suffix_under_boss_prefix() {
        let exec_id = "exec_18b44d2630b1df80_66";
        let branch = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
        // Must start with "boss/" and have an 8-char hex suffix.
        assert!(branch.starts_with("boss/"), "OpaqueHash branch must start with boss/");
        let suffix = branch.strip_prefix("boss/").unwrap();
        assert_eq!(suffix.len(), 8, "OpaqueHash suffix must be 8 hex chars, got: {suffix}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "OpaqueHash suffix must be hex digits, got: {suffix}",
        );
        // Must NOT expose the execution id.
        assert!(!branch.contains(exec_id), "OpaqueHash must not embed the execution id");
    }

    #[test]
    fn opaque_hash_is_deterministic_for_same_execution_id() {
        let exec_id = "exec_18b44d2630b1df80_66";
        let a = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
        let b = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
        assert_eq!(a, b, "OpaqueHash must be deterministic for the same execution id");
    }

    #[test]
    fn opaque_hash_differs_for_different_execution_ids() {
        let a = expected_branch_name("exec_aaaa0000_01", &BranchNaming::OpaqueHash, None);
        let b = expected_branch_name("exec_bbbb1111_02", &BranchNaming::OpaqueHash, None);
        assert_ne!(a, b, "distinct execution ids must produce distinct OpaqueHash branches");
    }

    #[test]
    fn custom_prefix_uses_prefix_and_opaque_hash_suffix() {
        let exec_id = "exec_18b44d2630b1df80_66";
        let branch = expected_branch_name(
            exec_id,
            &BranchNaming::CustomPrefix { prefix: "bduff".to_owned() },
            None,
        );
        assert!(branch.starts_with("bduff/"), "CustomPrefix branch must start with the given prefix");
        let suffix = branch.strip_prefix("bduff/").unwrap();
        assert_eq!(suffix.len(), 8, "CustomPrefix suffix must be 8 hex chars, got: {suffix}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "CustomPrefix suffix must be hex digits, got: {suffix}",
        );
        // Must NOT expose the execution id.
        assert!(!branch.contains(exec_id), "CustomPrefix must not embed the execution id");
    }

    #[test]
    fn custom_prefix_with_same_exec_id_differs_from_opaque_hash() {
        let exec_id = "exec_18b44d2630b1df80_66";
        let opaque = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
        let custom = expected_branch_name(
            exec_id,
            &BranchNaming::CustomPrefix { prefix: "bduff".to_owned() },
            None,
        );
        // Same hash suffix but different prefix → different branch names.
        assert_ne!(opaque, custom);
        // The hash suffix is the same (both derive from the same execution id).
        let opaque_hash = opaque.strip_prefix("boss/").unwrap();
        let custom_hash = custom.strip_prefix("bduff/").unwrap();
        assert_eq!(opaque_hash, custom_hash, "same execution id → same hash suffix");
    }

    #[test]
    fn branch_work_item_suffix_strips_the_prefix() {
        // `boss/` prefix → the execution id is the suffix.
        assert_eq!(
            branch_work_item_suffix("boss/exec_18b5023342a35418_18"),
            "exec_18b5023342a35418_18",
        );
        // A product `worker_branch_prefix` like `bduff/` → same suffix.
        assert_eq!(
            branch_work_item_suffix("bduff/exec_18b5023342a35418_18"),
            "exec_18b5023342a35418_18",
        );
        // OpaqueHash / CustomPrefix → the hash is the suffix.
        assert_eq!(branch_work_item_suffix("boss/a7f3e9c2"), "a7f3e9c2");
        assert_eq!(branch_work_item_suffix("bduff/a7f3e9c2"), "a7f3e9c2");
        // No slash → the whole string is the suffix.
        assert_eq!(branch_work_item_suffix("exec_x"), "exec_x");
        // Multi-segment → only the final segment counts.
        assert_eq!(branch_work_item_suffix("feature/x/exec_y"), "exec_y");
    }

    #[test]
    fn parse_api_pr_tsv_parses_all_six_fields() {
        let pr = parse_api_pr_tsv(
            "https://github.com/o/r/pull/7\topen\t2026-01-02T03:04:05Z\t3\t10\t4",
        )
        .expect("a non-empty url yields Some");
        assert_eq!(pr.url, "https://github.com/o/r/pull/7");
        assert_eq!(pr.state, "open");
        assert_eq!(pr.merged_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(pr.changed_files, 3);
        assert_eq!(pr.additions, 10);
        assert_eq!(pr.deletions, 4);
    }

    #[test]
    fn parse_api_pr_tsv_treats_null_and_empty_merged_at_as_none() {
        // jq emits a literal "null" (any case) when mergedAt is absent.
        let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull\t0\t0\t0").unwrap();
        assert_eq!(pr.merged_at, None);
        let pr = parse_api_pr_tsv("https://x/pull/1\topen\tNULL\t0\t0\t0").unwrap();
        assert_eq!(pr.merged_at, None);
        // An empty mergedAt column is likewise None.
        let pr = parse_api_pr_tsv("https://x/pull/1\topen\t\t0\t0\t0").unwrap();
        assert_eq!(pr.merged_at, None);
    }

    #[test]
    fn parse_api_pr_tsv_returns_none_when_url_empty() {
        // Empty leading field (the `select(.)` / absent-row case) → None.
        assert!(parse_api_pr_tsv("\topen\tnull\t0\t0\t0").is_none());
        assert!(parse_api_pr_tsv("").is_none());
    }

    #[test]
    fn parse_api_pr_tsv_defaults_missing_and_unparseable_numerics_to_zero() {
        // Missing trailing numeric columns fall back to 0.
        let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull").unwrap();
        assert_eq!((pr.changed_files, pr.additions, pr.deletions), (0, 0, 0));
        // Non-numeric junk also falls back to 0 (parse::<i64>().unwrap_or(0)).
        let pr = parse_api_pr_tsv("https://x/pull/1\topen\tnull\tx\ty\tz").unwrap();
        assert_eq!((pr.changed_files, pr.additions, pr.deletions), (0, 0, 0));
    }

    #[test]
    fn parse_api_pr_tsv_ignores_trailing_head_ref_field() {
        // The suffix-scan query appends a 7th headRefName column; the shared
        // parser must ignore it and still produce the same ApiPr. The call
        // site parses headRefName separately for the suffix filter.
        let line = "https://x/pull/9\topen\tnull\t1\t2\t3\tbduff/exec_abc";
        let pr = parse_api_pr_tsv(line).unwrap();
        assert_eq!(pr.url, "https://x/pull/9");
        assert_eq!(pr.changed_files, 1);
        assert_eq!(pr.deletions, 3);
        assert_eq!(line.split('\t').nth(6), Some("bduff/exec_abc"));
    }

    #[test]
    fn branches_identify_same_work_item_is_prefix_agnostic() {
        // The core of issue #1145: a `bduff/<suffix>` PR must associate
        // with the engine's `boss/<suffix>` expected branch.
        assert!(branches_identify_same_work_item(
            "bduff/exec_18b5023342a35418_18",
            "boss/exec_18b5023342a35418_18",
        ));
        // Identical branches still match.
        assert!(branches_identify_same_work_item(
            "boss/exec_x",
            "boss/exec_x",
        ));
        // Hash-suffix strategies match across prefixes too.
        assert!(branches_identify_same_work_item("bduff/a7f3e9c2", "boss/a7f3e9c2"));
        // Different suffixes (the incident's #1004 case:
        // `bduff/go-lib-publish-idempotent-v2` vs the work item's
        // `exec_…` suffix) correctly do NOT match.
        assert!(!branches_identify_same_work_item(
            "bduff/go-lib-publish-idempotent-v2",
            "boss/exec_18b5023342a35418_18",
        ));
        // Defensive: empty suffixes (malformed `…/` branches) never match,
        // even each other.
        assert!(!branches_identify_same_work_item("boss/", "bduff/"));
    }

    /// R6 invariant: the cold-path detector scopes its `gh pr list --head`
    /// query by `repo_remote_url`, so two executions on *different* products
    /// (and therefore different repos) that happen to produce the same
    /// OpaqueHash suffix do NOT collide — the query only returns PRs in the
    /// execution's own repo.
    #[tokio::test]
    async fn opaque_hash_collision_across_repos_does_not_mislead_detector() {
        // We can't force a real hash collision in unit-test time. Instead we
        // verify the scoping invariant: two executions on different repos are
        // each queried independently, and a PR found in repo-A's namespace is
        // not attributed to an execution in repo-B.
        let workspace = tempdir().unwrap();
        // Build a product and a chore for repo-A.
        let (db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
        let repo_a = "git@github.com:spinyfin/mono.git";
        let repo_b = "git@github.com:otherorg/otherrepo.git";

        // Detector for repo-A always finds the expected PR.
        let detector_a = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/10"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector_a.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        // Verify the detector was called with the execution's own repo_remote_url.
        let calls = detector_a.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].repo_remote_url, repo_a,
            "detector must be scoped to the execution's repo, not any other",
        );
        // repo_b must never appear in any detect_pr call.
        assert!(
            calls.iter().all(|c| c.repo_remote_url != repo_b),
            "detector must never query repo-B when the execution belongs to repo-A",
        );
        let _ = outcome;
    }

    /// Acceptance: `branch_naming` snapshotted at spawn is used by the
    /// cold-path detector to reconstruct the branch name. An execution with
    /// `BranchNaming::OpaqueHash` calls the detector with an opaque-hash
    /// branch name, not the classic `boss/exec_<id>` form.
    #[tokio::test]
    async fn detector_uses_branch_naming_from_execution_row() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id) = fixture(workspace.path());

        // Patch the execution's branch_naming to OpaqueHash so we can verify
        // the detector is called with the opaque-hash branch form.
        db.force_branch_naming_for_test(&execution_id, &BranchNaming::OpaqueHash)
            .unwrap();

        let expected_hash_branch = expected_branch_name(&execution_id, &BranchNaming::OpaqueHash, None);
        let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/77"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector.clone(),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        // P992 task 7: chore_implementation holds task and enqueues reviewer.
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "expected ReviewerEnqueued; got {outcome:?}",
        );

        let calls = detector.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].expected_branch, expected_hash_branch,
            "detector must use the opaque-hash branch name from the execution row",
        );
        assert!(
            !calls[0].expected_branch.contains(&execution_id),
            "opaque-hash branch must not embed the execution id",
        );
    }

    // ── P992 task 10: no-op / trivial-diff skip gate ──────────────────────────
    //
    // Helper that creates a fixture with `last_reviewed_sha` already set on the
    // chore (simulating a prior review cycle) and stages a PR URL, then returns
    // everything needed to drive `on_stop` in a test.
    fn noop_skip_fixture(
        workspace_path: &Path,
        last_reviewed_sha: Option<&str>,
    ) -> (
        Arc<WorkDb>,
        String, // chore_id
        String, // execution_id
        Arc<crate::pr_url_capture::StagedPrUrlCache>,
        String, // expected_branch
    ) {
        const PR_URL: &str = "https://github.com/spinyfin/mono/pull/88";
        let (db, _product_id, chore_id, execution_id) = fixture(workspace_path);
        if let Some(sha) = last_reviewed_sha {
            db.increment_task_review_cycle(&chore_id, Some(sha))
                .expect("failed to set last_reviewed_sha");
        }
        let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged.record_if_unset(&execution_id, PR_URL);
        let branch = expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None);
        (db, chore_id, execution_id, staged, branch)
    }

    /// First review of a PR is never skipped by the trivial rule (design §8).
    /// When `last_reviewed_sha` is `None` (review_cycle = 0) the gate must
    /// pass through and enqueue the reviewer regardless of the head OID or
    /// diff size.
    #[tokio::test]
    async fn noop_skip_gate_first_review_never_skipped() {
        let workspace = tempdir().unwrap();
        // last_reviewed_sha = None → first review
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), None);

        let verifier = StubBranchVerifier::ok(&branch);
        // Return a 0-line diff — if the gate were applied, this would trigger a skip.
        // The first-review guard must prevent that.
        verifier.set_diff_line_count(Ok(0)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "first review must never be skipped; expected ReviewerEnqueued, got {outcome:?}",
        );
    }

    /// When the current PR head SHA equals `last_reviewed_sha` the gate skips
    /// the reviewer and advances the task directly to in_review.
    #[tokio::test]
    async fn noop_skip_gate_skips_when_sha_unchanged() {
        const SAME_SHA: &str = "sha_abc123";
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some(SAME_SHA));

        let verifier = StubBranchVerifier::ok(&branch);
        // Current head == last_reviewed_sha → skip.
        verifier.set_head_oid(Ok(SAME_SHA.to_owned())).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "sha_unchanged must skip reviewer; expected PrDetected, got {outcome:?}",
        );
    }

    /// When the effective diff between last-reviewed and current head is zero
    /// lines (pure rebase with no file-content changes) the gate skips the
    /// reviewer.
    #[tokio::test]
    async fn noop_skip_gate_skips_on_empty_diff() {
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some("sha_old"));

        let verifier = StubBranchVerifier::ok(&branch);
        // Different head SHA (new commit) but 0 changed lines → pure rebase.
        verifier.set_head_oid(Ok("sha_new".to_owned())).await;
        verifier.set_diff_line_count(Ok(0)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "empty diff must skip reviewer; expected PrDetected, got {outcome:?}",
        );
    }

    /// When `min_review_changed_lines > 0` and the diff is below the threshold
    /// the gate skips the reviewer (trivial-diff path).
    #[tokio::test]
    async fn noop_skip_gate_skips_trivial_diff_when_threshold_set() {
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some("sha_old"));

        let verifier = StubBranchVerifier::ok(&branch);
        verifier.set_head_oid(Ok("sha_new".to_owned())).await;
        // 5 changed lines, threshold is 10 → trivial → skip.
        verifier.set_diff_line_count(Ok(5)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_min_review_changed_lines(10);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "trivial diff below threshold must skip reviewer; expected PrDetected, got {outcome:?}",
        );
    }

    /// When `min_review_changed_lines > 0` and the diff meets the threshold
    /// the reviewer is enqueued normally.
    #[tokio::test]
    async fn noop_skip_gate_does_not_skip_when_diff_meets_threshold() {
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some("sha_old"));

        let verifier = StubBranchVerifier::ok(&branch);
        verifier.set_head_oid(Ok("sha_new".to_owned())).await;
        // 10 changed lines, threshold is 10 → not trivial → review.
        verifier.set_diff_line_count(Ok(10)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_min_review_changed_lines(10);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "diff at threshold must not skip reviewer; expected ReviewerEnqueued, got {outcome:?}",
        );
    }

    /// The default `min_review_changed_lines = 0` must NOT skip a small but
    /// non-empty diff (only empty diffs and SHA matches are skipped by default).
    #[tokio::test]
    async fn noop_skip_gate_default_does_not_skip_nonzero_diff() {
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some("sha_old"));

        let verifier = StubBranchVerifier::ok(&branch);
        verifier.set_head_oid(Ok("sha_new".to_owned())).await;
        // 1 changed line — with the conservative default (0 threshold) this
        // must NOT be treated as trivial.
        verifier.set_diff_line_count(Ok(1)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);
        // min_review_changed_lines uses the default (0 = disabled)

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "1-line diff with default threshold must not skip; expected ReviewerEnqueued, got {outcome:?}",
        );
    }

    /// If `fetch_pr_head_oid` fails the gate fails open (proceeds with review),
    /// so a transient GitHub API error never silently suppresses a reviewer pass.
    #[tokio::test]
    async fn noop_skip_gate_fails_open_on_head_oid_error() {
        let workspace = tempdir().unwrap();
        let (db, _chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), Some("sha_old"));

        let verifier = StubBranchVerifier::ok(&branch);
        // Simulate a GitHub API failure when fetching the PR head OID.
        verifier
            .set_head_oid(Err("simulated API error".to_owned()))
            .await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier);

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "API error in noop gate must fail open (enqueue reviewer); got {outcome:?}",
        );
    }

    // ── P992 task 13: end-to-end / integration tests ──────────────────────────
    //
    // These tests exercise the complete produce→review→revise→re-review loop
    // and the termination conditions (cycle bound, no-op gate interaction).
    // Individual component unit tests (severity gate, no-op gate, instructions
    // rendering, etc.) live above; these tests operate at the completion-handler
    // level to verify the full state-machine transitions.

    /// Build a JSONL transcript line containing `review_result_json` in an
    /// assistant message, matching the format `read_final_triage_message`
    /// expects (one JSON object per line, `type=assistant`, `message.content`
    /// array with a `text` block).
    ///
    /// Uses `serde_json` for the outer object so the `text` field is properly
    /// escaped regardless of what characters appear in the ReviewResult JSON.
    fn make_review_transcript_jsonl(review_result_json: &str) -> String {
        let text =
            format!("Here is my automated PR review.\n\n```json\n{review_result_json}\n```");
        let obj = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": text}]
            }
        });
        format!("{}\n", obj)
    }

    /// Build a producing chore with `pr_url` already set (simulating the
    /// PendingReview state that `finalize_pr_transition` writes) together with
    /// a `pr_review` execution in `waiting_human` status. Optionally write a
    /// JSONL transcript file and register its path so
    /// `finalize_pr_review_pass` can read the `ReviewResult`.
    ///
    /// Returns `(db, product_id, chore_id, pr_review_exec_id, pr_url)`.
    fn pr_review_exec_fixture(
        workspace_path: &Path,
        review_result_json: Option<&str>,
    ) -> (Arc<WorkDb>, String, String, String, String) {
        const PR_URL: &str = "https://github.com/spinyfin/mono/pull/88";
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        // Producing task — starts active, gets pr_url stamped so the reviewer
        // can find the PR (mirrors what finalize_pr_transition writes on
        // PendingReview).
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Implement feature X".into(),
                description: Some("Feature X adds Y functionality to the pipeline.".into()),
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            crate::work::WorkItemPatch {
                pr_url: Some(PR_URL.into()),
                ..Default::default()
            },
        )
        .unwrap();

        // PrReview execution in waiting_human (reviewer spawned, now stopped).
        let pr_review_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let (pr_review_exec, run) = db
            .start_execution_run(
                &pr_review_exec.id,
                "review-worker-1",
                "mono",
                "lease-review-1",
                "mono-agent-review-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &pr_review_exec.id,
                &run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("reviewer spawned"),
                None,
                false,
                None,
            )
            .unwrap();

        // Optionally write a transcript containing the ReviewResult JSON.
        if let Some(json) = review_result_json {
            let jsonl = make_review_transcript_jsonl(json);
            let transcript_path =
                workspace_path.join(format!("transcript-{}.jsonl", pr_review_exec.id));
            std::fs::write(&transcript_path, jsonl.as_bytes()).unwrap();
            db.set_run_transcript_path_if_unset(
                &pr_review_exec.id,
                transcript_path.to_str().unwrap(),
            )
            .unwrap();
        }

        (
            db,
            product.id,
            chore.id,
            pr_review_exec.id,
            PR_URL.to_owned(),
        )
    }

    /// Produce a minimal valid `ReviewResult` JSON with no qualifying findings
    /// (medium severity only, no regressions) — the engine severity gate must
    /// NOT fire for this result.
    fn clean_review_result_json(pr_url: &str) -> String {
        serde_json::json!({
            "pr_url": pr_url,
            "head_sha": "sha_reviewed_abc123",
            "summary": "The PR looks good overall. Minor style note only.",
            "revision_warranted": false,
            "findings": [
                {
                    "severity": "medium",
                    "category": "readability",
                    "file": "src/lib.rs",
                    "title": "Minor naming nit",
                    "detail": "Consider renaming `x` to `input` for clarity.",
                    "confidence": "low"
                }
            ],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        })
        .to_string()
    }

    /// Produce a `ReviewResult` JSON with a HIGH severity correctness finding —
    /// the engine severity gate fires for this result.
    fn high_finding_review_result_json(pr_url: &str) -> String {
        serde_json::json!({
            "pr_url": pr_url,
            "head_sha": "sha_reviewed_abc123",
            "summary": "Critical correctness issue found in the PR.",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "src/pr.rs",
                    "location": "fn ensure_pr, ~L120",
                    "title": "Duplicate PR case not handled",
                    "detail": "The `?` on the gh call swallows the 422 — handle the duplicate-PR case explicitly.",
                    "confidence": "high"
                }
            ],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        })
        .to_string()
    }

    /// Produce a `ReviewResult` JSON with a LOW severity REGRESSION finding
    /// (the T793 check class). Even though the severity is low, the engine's
    /// gate must fire because `category = "regression"` overrides severity.
    fn t793_regression_review_result_json(pr_url: &str) -> String {
        serde_json::json!({
            "pr_url": pr_url,
            "head_sha": "sha_reviewed_abc123",
            "summary": "Forward-port silently dropped the autostart feature.",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "low",
                    "category": "regression",
                    "file": "tools/boss/engine/core/src/lib.rs",
                    "location": "fn init, ~L10",
                    "title": "Forward-port dropped the autostart feature",
                    "detail": "The autostart flag was removed during conflict resolution; restore it.",
                    "confidence": "high"
                }
            ],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        })
        .to_string()
    }

    // ── Tests: finalize_pr_review_pass paths ─────────────────────────────────

    /// Build a `FakePrStateChecker` that always reports the PR as open — used
    /// by all pr_review tests so `create_revision` doesn't shell out to `gh`.
    fn open_pr_checker() -> Arc<dyn crate::work::PrStateChecker> {
        Arc::new(FakePrStateChecker::always(PrOpenState::Open))
    }

    /// A clean reviewer result (no critical/high/regression findings) must
    /// advance the producing task to `in_review` without creating a revision
    /// and tick the `review_cycle` counter.
    #[tokio::test]
    async fn pr_review_pass_clean_advances_to_in_review_without_revision() {
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/88";
        let json = clean_review_result_json(pr_url);
        let (db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
            pr_review_exec_fixture(workspace.path(), Some(&json));

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_pr_state_checker(open_pr_checker());

        let outcome = handler.on_stop(&pr_review_exec_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewPassCompleted { .. }),
            "clean result must yield ReviewPassCompleted; got {outcome:?}",
        );

        // Producing task must be in in_review.
        let item = db.get_work_item(&chore_id).unwrap();
        let task = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected task/chore, got {other:?}"),
        };
        assert_eq!(task.status, TaskStatus::InReview, "chore must advance to in_review after reviewer approves");

        // review_cycle must be incremented (0 → 1) by the completion handler.
        let (review_cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
        assert_eq!(review_cycle, 1, "review_cycle must be incremented after each reviewer pass");
        assert_eq!(
            last_sha.as_deref(),
            Some("sha_reviewed_abc123"),
            "last_reviewed_sha must be recorded from the ReviewResult head_sha",
        );
    }

    /// A `ReviewResult` with a HIGH severity finding must trigger the engine's
    /// severity gate and create a revision on the producing task with the
    /// correct `created_via` prefix and rendered instructions.
    #[tokio::test]
    async fn pr_review_pass_high_finding_creates_revision_with_correct_metadata() {
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/88";
        let json = high_finding_review_result_json(pr_url);
        let (db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
            pr_review_exec_fixture(workspace.path(), Some(&json));

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_pr_state_checker(open_pr_checker());

        let outcome = handler.on_stop(&pr_review_exec_id).await;
        let revision_task_id = match &outcome {
            StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => {
                revision_task_id.clone()
            }
            other => panic!("high finding must yield ReviewPassRevisionCreated; got {other:?}"),
        };

        // Revision must have the pr_review created_via prefix so the
        // RevisionImplementation completion triggers another reviewer pass.
        let revision = match db.get_work_item(&revision_task_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            other => panic!("revision is not a task/chore: {other:?}"),
        };
        assert!(
            revision.created_via.starts_with(boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX),
            "revision created_via must carry the pr_review prefix so the \
             RevisionImplementation re-triggers a reviewer pass; got: {:?}",
            revision.created_via,
        );
        // Revision instructions must mention the finding.
        assert!(
            revision.description.contains("Duplicate PR case not handled"),
            "revision instructions must include the finding title; got: {:?}",
            revision.description,
        );

        // Producing task advances to in_review even when a revision is created
        // (the revision is a follow-up child — the PR is still ready for
        // human review with the outstanding findings noted internally).
        let item = db.get_work_item(&chore_id).unwrap();
        let task = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            task.status, TaskStatus::InReview,
            "producing task must advance to in_review after reviewer pass",
        );
    }

    /// A `ReviewResult` with a `regression` category finding must trigger the
    /// engine's severity gate *regardless of severity level* — this is the
    /// T793 check (a live feature silently removed during a forward-port must
    /// be caught even if the reviewer rates it `low` severity).
    #[tokio::test]
    async fn pr_review_regression_finding_creates_revision_at_low_severity_t793_check() {
        let workspace = tempdir().unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/88";
        let json = t793_regression_review_result_json(pr_url);
        let (db, _product_id, _chore_id, pr_review_exec_id, _pr_url) =
            pr_review_exec_fixture(workspace.path(), Some(&json));

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_pr_state_checker(open_pr_checker());

        let outcome = handler.on_stop(&pr_review_exec_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewPassRevisionCreated { .. }),
            "low-severity regression finding must still fire the severity gate \
             and create a revision (T793 check); got {outcome:?}",
        );
    }

    /// When no transcript is recorded (reviewer crashed or hook missed) the
    /// completion handler must fall back gracefully to advancing the producing
    /// task to `in_review` without creating a revision.
    #[tokio::test]
    async fn pr_review_pass_missing_transcript_advances_gracefully_without_revision() {
        let workspace = tempdir().unwrap();
        // No review result JSON → no transcript written.
        let (db, _product_id, chore_id, pr_review_exec_id, _pr_url) =
            pr_review_exec_fixture(workspace.path(), None);

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_pr_state_checker(open_pr_checker());

        let outcome = handler.on_stop(&pr_review_exec_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewPassCompleted { .. }),
            "missing transcript must fall back to ReviewPassCompleted; got {outcome:?}",
        );

        // Task still advances — a reviewer crash must not strand the task.
        let item = db.get_work_item(&chore_id).unwrap();
        let task = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(
            task.status, TaskStatus::InReview,
            "producing task must advance to in_review even when reviewer produced no transcript",
        );
    }

    // ── Test: cycle bound ─────────────────────────────────────────────────────

    /// When `review_cycle` has already reached `max_review_cycles`, the next
    /// producing-worker completion must skip the reviewer entirely, advance the
    /// task directly to `in_review` (PrDetected), and create a sticky
    /// `pr_review_cycle_bound` attention item for the human.
    #[tokio::test]
    async fn pr_review_cycle_bound_skips_reviewer_and_creates_attention_item() {
        let workspace = tempdir().unwrap();
        let (db, chore_id, execution_id, staged, branch) =
            noop_skip_fixture(workspace.path(), None);

        // Pre-increment the cycle counter to `max_review_cycles` so the bound
        // is already reached when the producing worker finishes.
        let max_cycles: usize = 1;
        for _ in 0..max_cycles {
            db.increment_task_review_cycle(&chore_id, Some("sha_prev"))
                .expect("failed to pre-increment review_cycle");
        }

        let verifier = StubBranchVerifier::ok(&branch);
        // diff line count doesn't matter here (cycle bound fires before noop gate).
        verifier.set_diff_line_count(Ok(999)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged)
        .with_branch_verifier(verifier)
        .with_max_review_cycles(max_cycles);

        let outcome = handler.on_stop(&execution_id).await;
        // Cycle bound: no reviewer enqueued → task goes straight to in_review.
        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "cycle bound must skip reviewer and yield PrDetected; got {outcome:?}",
        );

        // Verify the sticky attention item was created for the human.
        let item = db.get_work_item(&chore_id).unwrap();
        let task = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, TaskStatus::InReview, "task must be in_review after cycle bound");

        // The attention item is created on the task (work_item_id), not on
        // the execution, so we query it via the task.
        let attentions = db
            .list_attention_items_for_work_item(&chore_id)
            .expect("failed to list attention items");
        assert!(
            attentions.iter().any(|a| a.kind == "pr_review_cycle_bound"),
            "a pr_review_cycle_bound attention item must exist; got: {attentions:?}",
        );
    }

    // ── Test: full produce → review → revise → re-review loop ────────────────

    /// End-to-end integration test for the complete automated-reviewer loop
    /// (P992 design §1, §4, §7, §8, §9, §10, task 13).
    ///
    /// Flow:
    ///   1. ChoreImplementation finishes → reviewer enqueued (PendingReview).
    ///   2. PrReview (high finding) → revision created; producing task in_review.
    ///   3. RevisionImplementation finishes → reviewer re-enqueued.
    ///   4. PrReview (clean) → ReviewPassCompleted; revision task in_review.
    #[tokio::test]
    async fn full_produce_review_revise_re_review_loop_converges() {
        const PR_URL: &str = "https://github.com/spinyfin/mono/pull/99";
        let workspace = tempdir().unwrap();

        // ── Step 1: ChoreImplementation completes → reviewer enqueued ────────
        let (db, _product_id, chore_id, chore_exec_id) = fixture(workspace.path());

        let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged.record_if_unset(&chore_exec_id, PR_URL);

        let chore_branch =
            expected_branch_name(&chore_exec_id, &BranchNaming::BossExecPrefix, None);
        let verifier = StubBranchVerifier::ok(&chore_branch);
        // diff line count: non-trivial so no-op gate doesn't fire (first review
        // is never skipped by the trivial rule, but set it anyway for realism).
        verifier.set_diff_line_count(Ok(50)).await;

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(staged.clone())
        .with_branch_verifier(verifier.clone())
        .with_pr_state_checker(open_pr_checker());

        let outcome = handler.on_stop(&chore_exec_id).await;
        assert!(
            matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
            "step 1: expected ReviewerEnqueued; got {outcome:?}",
        );

        // ── Step 2: PrReview (high finding) → revision created ───────────────
        // Find the newly-created PrReview execution (status = ready).
        let ready = db.list_ready_executions().unwrap();
        let pr_review_exec_1 = ready
            .iter()
            .find(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == chore_id)
            .cloned()
            .expect("a PrReview execution must exist in ready status after step 1");

        // Start + finish the PrReview execution (simulate reviewer spawned).
        let (pr_review_exec_1, run1) = db
            .start_execution_run(
                &pr_review_exec_1.id,
                "review-worker-1",
                "mono",
                "lease-review-1",
                "mono-agent-review-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &pr_review_exec_1.id,
                &run1.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("reviewer spawned"),
                None,
                false,
                None,
            )
            .unwrap();

        // Write a transcript with a HIGH finding.
        let high_json = high_finding_review_result_json(PR_URL);
        let transcript1 = workspace
            .path()
            .join(format!("transcript-{}.jsonl", pr_review_exec_1.id));
        std::fs::write(&transcript1, make_review_transcript_jsonl(&high_json).as_bytes()).unwrap();
        db.set_run_transcript_path_if_unset(
            &pr_review_exec_1.id,
            transcript1.to_str().unwrap(),
        )
        .unwrap();

        let outcome2 = handler.on_stop(&pr_review_exec_1.id).await;
        let revision_task_id = match &outcome2 {
            StopOutcome::ReviewPassRevisionCreated { revision_task_id, .. } => {
                revision_task_id.clone()
            }
            other => panic!("step 2: expected ReviewPassRevisionCreated; got {other:?}"),
        };

        // Verify the chore is now in_review and review_cycle = 1.
        let chore_item = db.get_work_item(&chore_id).unwrap();
        let chore_task = match chore_item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(chore_task.status, TaskStatus::InReview, "step 2: chore must be in_review");
        let (cycle_after_r1, _) = db.get_task_review_cycle_state(&chore_id).unwrap();
        assert_eq!(cycle_after_r1, 1, "step 2: review_cycle must be 1 after first reviewer pass");

        // ── Step 3: RevisionImplementation finishes → reviewer re-enqueued ───
        // Create and run a RevisionImplementation execution for the revision task.
        let rev_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision_task_id.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let (rev_exec, rev_run) = db
            .start_execution_run(
                &rev_exec.id,
                "worker-rev-1",
                "mono",
                "lease-rev-1",
                "mono-agent-rev-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &rev_exec.id,
                &rev_run.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("revision worker spawned"),
                None,
                false,
                None,
            )
            .unwrap();

        // Stage the same PR URL for the revision execution.
        let rev_staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        rev_staged.record_if_unset(&rev_exec.id, PR_URL);

        let rev_branch = expected_branch_name(&rev_exec.id, &BranchNaming::BossExecPrefix, None);
        let rev_verifier = StubBranchVerifier::ok(&rev_branch);
        rev_verifier.set_diff_line_count(Ok(30)).await;

        let handler3 = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            Arc::new(StubCubeClient::default()),
            Arc::new(RecordingPublisher::default()),
            Arc::new(RecordingPaneReleaser::default()),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_staged_pr_urls(rev_staged)
        .with_branch_verifier(rev_verifier)
        .with_pr_state_checker(open_pr_checker());

        let outcome3 = handler3.on_stop(&rev_exec.id).await;
        assert!(
            matches!(outcome3, StopOutcome::ReviewerEnqueued { .. }),
            "step 3: revision completion must re-enqueue reviewer; got {outcome3:?}",
        );

        // ── Step 4: PrReview (clean) → ReviewPassCompleted ───────────────────
        // Find the second PrReview execution (for the revision task).
        let ready2 = db.list_ready_executions().unwrap();
        let pr_review_exec_2 = ready2
            .iter()
            .find(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == revision_task_id)
            .cloned()
            .expect("a second PrReview execution must exist after step 3");

        // Start + finish.
        let (pr_review_exec_2, run2) = db
            .start_execution_run(
                &pr_review_exec_2.id,
                "review-worker-2",
                "mono",
                "lease-review-2",
                "mono-agent-review-002",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &pr_review_exec_2.id,
                &run2.id,
                ExecutionStatus::WaitingHuman,
                "completed",
                Some("reviewer 2 spawned"),
                None,
                false,
                None,
            )
            .unwrap();

        // Write a clean transcript — no qualifying findings.
        let clean_json = clean_review_result_json(PR_URL);
        let transcript2 = workspace
            .path()
            .join(format!("transcript-{}.jsonl", pr_review_exec_2.id));
        std::fs::write(&transcript2, make_review_transcript_jsonl(&clean_json).as_bytes()).unwrap();
        db.set_run_transcript_path_if_unset(
            &pr_review_exec_2.id,
            transcript2.to_str().unwrap(),
        )
        .unwrap();

        let outcome4 = handler.on_stop(&pr_review_exec_2.id).await;
        assert!(
            matches!(outcome4, StopOutcome::ReviewPassCompleted { .. }),
            "step 4: clean review must yield ReviewPassCompleted; got {outcome4:?}",
        );

        // Revision task must be in_review — the loop converged.
        let rev_item = db.get_work_item(&revision_task_id).unwrap();
        let rev_task = match rev_item {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            other => panic!("expected task, got {other:?}"),
        };
        assert_eq!(
            rev_task.status, TaskStatus::InReview,
            "step 4: revision task must be in_review after clean reviewer pass",
        );
    }
}
