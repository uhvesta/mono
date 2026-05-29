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

use boss_protocol::FrontendEvent;

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::design_detector;
use crate::merge_poller::{MergeProbe, NoopMergeProbe, update_pr_poll_state};
use crate::metrics::Registry;
use crate::nudge_breaker::{DEFAULT_MAX_UNPRODUCTIVE_NUDGES, NudgeBreaker, NudgeDecision};
use crate::work::{
    CreateAttentionItemInput, PendingMergeCheck, WorkDb, WorkItem, WorkerPrCompletionTarget,
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

/// Asks the registered app session to tear down the libghostty pane
/// hosting `run_id`. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
/// The completion handler calls this after a successful cube lease
/// release on PR detection so the Workers grid pane disappears.
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str);
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs).
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) {}
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

/// Default worker branch-name prefix when a product carries no
/// `worker_branch_prefix` override. Preserves the historical
/// `boss/exec_<id>` shape so existing setups are unchanged.
pub const DEFAULT_WORKER_BRANCH_PREFIX: &str = "boss/";

/// Engine-supplied branch name a worker must push to when opening
/// the PR for an execution. The shape is
/// `<worker_branch_prefix>exec_<id>`: the `exec_<id>` suffix is
/// derived deterministically from `execution_id` so the detector can
/// reconstruct the expected name from `state.db` alone — no local jj
/// reads, no shared-store contamination — and is the stable
/// identifier every subsystem keys off. Only the leading prefix is
/// configurable (per-product, via `Product::worker_branch_prefix`,
/// frozen onto the execution row at spawn). `worker_branch_prefix` of
/// `None` falls back to [`DEFAULT_WORKER_BRANCH_PREFIX`].
///
/// See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for
/// the rationale: a per-execution branch name gives the detector a
/// signal that is unique by construction. Sibling workers in other
/// cube workspaces have different execution IDs and therefore push
/// to different branches, so a branch-keyed `gh pr list --head <name>`
/// query cannot misattribute their PRs to this execution. The
/// configurable prefix does not weaken this: the `exec_<id>` suffix
/// remains unique per execution regardless of prefix.
pub fn expected_branch_name(worker_branch_prefix: Option<&str>, execution_id: &str) -> String {
    let prefix = worker_branch_prefix.unwrap_or(DEFAULT_WORKER_BRANCH_PREFIX);
    format!("{prefix}{execution_id}")
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
                tracing::debug!(
                    repo = %repo_slug,
                    branch = %expected_branch,
                    "pr_detect: no PR found for expected branch; returning None",
                );
                return Ok(PrStatus::None);
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
        fetch_pr_head_oid_cmd(repo_slug, pr_number).await
    }
}

/// Shell out to `gh pr view <pr_number> -R <repo_slug> --json headRefName`
/// and return the branch name, or an error on failure / empty response.
async fn fetch_pr_head_ref_cmd(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefName",
            "--jq",
            ".headRefName",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh pr view {pr_number} -R {repo_slug}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh pr view {pr_number} -R {repo_slug}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let head_ref = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if head_ref.is_empty() {
        return Err(anyhow!("empty headRefName for PR {pr_number} in {repo_slug}"));
    }
    Ok(head_ref)
}

/// Shell out to `gh pr view <pr_number> -R <repo_slug> --json headRefOid`
/// and return the head SHA, or an error on failure / empty response.
async fn fetch_pr_head_oid_cmd(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!("failed to spawn `gh pr view {pr_number} -R {repo_slug} --json headRefOid`")
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh pr view {pr_number} -R {repo_slug} --json headRefOid` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let head_oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if head_oid.is_empty() {
        return Err(anyhow!("empty headRefOid for PR {pr_number} in {repo_slug}"));
    }
    Ok(head_oid)
}

/// Parse the PR number from a canonical GitHub PR URL
/// (`https://github.com/<owner>/<repo>/pull/<N>`).
pub(crate) fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    pr_url.split('/').last()?.parse().ok()
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
    let output = Command::new("gh")
        .args([
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
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!("failed to spawn `gh pr list -R {repo_slug} --head {branch}`")
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh pr list -R {repo_slug} --head {branch}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut parts = trimmed.split('\t');
    let url = parts.next().unwrap_or("").trim().to_owned();
    let state = parts.next().unwrap_or("").trim().to_owned();
    let merged_at_raw = parts.next().unwrap_or("").trim();
    let changed_files_raw = parts.next().unwrap_or("0").trim();
    let additions_raw = parts.next().unwrap_or("0").trim();
    let deletions_raw = parts.next().unwrap_or("0").trim();
    if url.is_empty() {
        return Ok(None);
    }
    let merged_at = if merged_at_raw.is_empty() || merged_at_raw.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(merged_at_raw.to_owned())
    };
    let changed_files = changed_files_raw.parse::<i64>().unwrap_or(0);
    let additions = additions_raw.parse::<i64>().unwrap_or(0);
    let deletions = deletions_raw.parse::<i64>().unwrap_or(0);
    Ok(Some(ApiPr {
        url,
        state,
        merged_at,
        changed_files,
        additions,
        deletions,
    }))
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
        .last()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("cannot parse PR number from URL: {pr_url}"))?;
    let endpoint = format!("repos/{repo_slug}/pulls/{pr_number}");
    let output = Command::new("gh")
        .args([
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            "((.additions // 0) + (.deletions // 0))",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh api {endpoint}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh api {endpoint}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let total: i64 = stdout.trim().parse().with_context(|| {
        format!("unexpected output from `gh api {endpoint}`: {:?}", stdout.trim())
    })?;
    Ok(total > 0)
}

/// Pull `owner/repo` out of a remote URL. Handles both SSH
/// (`git@github.com:owner/repo.git`) and HTTPS
/// (`https://github.com/owner/repo[.git]`) shapes.
pub(crate) fn parse_repo_slug(remote_url: &str) -> Result<String> {
    let trimmed = remote_url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let (_, after_host) = trimmed
        .split_once("github.com")
        .ok_or_else(|| anyhow!("not a github.com URL: {remote_url}"))?;
    let after_host = after_host.trim_start_matches([':', '/']);
    let mut slash_iter = after_host.splitn(3, '/');
    let owner = slash_iter
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing owner segment: {remote_url}"))?;
    let repo = slash_iter
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing repo segment: {remote_url}"))?;
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
    /// Primary-path resolution-signal staging for `conflict_resolution`
    /// executions. Populated by the `PostToolUse` dispatcher in `app.rs`
    /// when a Bash event is a force-push or a PR-comment post. On Stop,
    /// `on_stop` checks this cache first: if any signal is present it
    /// transitions the parent chore `blocked → in_review` immediately,
    /// without waiting for the merge-poller sweep to notice GitHub now
    /// reports the PR as `MERGEABLE`. Defaults to an empty cache so tests
    /// that don't exercise the signal path fall through to the catch-all
    /// finalizer unchanged.
    staged_resolution_signals:
        Arc<crate::resolution_signal_capture::StagedResolutionSignalCache>,
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
            staged_resolution_signals: Arc::new(
                crate::resolution_signal_capture::StagedResolutionSignalCache::new(),
            ),
            nudge_breaker: Arc::new(NudgeBreaker::new()),
            max_unproductive_nudges: DEFAULT_MAX_UNPRODUCTIVE_NUDGES,
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
    /// handler so the `PostToolUse` dispatcher and the on-Stop resolver
    /// share the same cache. `app.rs` calls this once after construction;
    /// tests that want to exercise the signal path can call it with their
    /// own cache. Tests that don't invoke it get the default empty cache
    /// and fall through to the existing catch-all finalizer — preserving
    /// pre-change behaviour without a signature break.
    pub fn with_staged_resolution_signals(
        mut self,
        cache: Arc<crate::resolution_signal_capture::StagedResolutionSignalCache>,
    ) -> Self {
        self.staged_resolution_signals = cache;
        self
    }

    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        let outcome = self.on_stop_inner(execution_id).await;
        // Phase 4 #11: for `conflict_resolution` executions, drive the
        // parent-chore transition and attempt finalization.
        //
        // Phase 10 #33: the same catch-all applies to `ci_remediation`
        // executions. The two flows share the on-Stop hook but write to
        // different attempt tables — dispatch by `execution.kind`.
        if let Ok(execution) = self.work_db.get_execution(execution_id) {
            match execution.kind.as_str() {
                "conflict_resolution" => {
                    // Primary path: at least one resolution signal was staged
                    // from `PostToolUse` Bash events (force-push or PR comment).
                    // Transition parent blocked → in_review immediately, without
                    // waiting for the merge-poller sweep to see GitHub report the
                    // PR as MERGEABLE. The catch-all finalizer below is idempotent
                    // (early-exits if the attempt is already terminal).
                    if self
                        .staged_resolution_signals
                        .has_any_signal(execution_id)
                    {
                        self.finalize_via_resolution_signal(&execution).await;
                        self.staged_resolution_signals.forget(execution_id);
                    }
                    // Catch-all finalizer: always runs so the attempt is marked
                    // `failed` when the worker exited without pushing (no signal
                    // staged, detector returned nothing). Idempotent — skips when
                    // the attempt is already in a terminal state.
                    self.finalize_conflict_resolution_attempt(&execution, &outcome)
                        .await;
                }
                "ci_remediation" => {
                    self.finalize_ci_remediation_attempt(&execution, &outcome)
                        .await;
                }
                _ => {}
            }
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
        if !matches!(execution.status.as_str(), "running" | "waiting_human") {
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
            let expected_branch =
                expected_branch_name(execution.worker_branch_prefix.as_deref(), execution_id);
            let repo_slug = parse_repo_slug(&execution.repo_remote_url);
            let branch_ok = match repo_slug {
                Ok(ref slug) => {
                    match pr_number_from_url(&staged_url) {
                        Some(pr_num) => {
                            match self.branch_verifier.fetch_pr_head_ref(slug, pr_num).await {
                                Ok(ref head_ref) if head_ref == &expected_branch => true,
                                Ok(head_ref) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        staged_pr_branch = %head_ref,
                                        %expected_branch,
                                        "pr_recheck_staged_branch_mismatch: staged PR branch does not match expected; dropping staged URL",
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
        if execution.status != "waiting_human" {
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

        let expected_branch =
            expected_branch_name(execution.worker_branch_prefix.as_deref(), &execution.id);
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
                if execution.kind == "ci_remediation" {
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
        if !matches!(execution.status.as_str(), "running" | "waiting_human") {
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
            let expected_branch =
                expected_branch_name(execution.worker_branch_prefix.as_deref(), execution_id);
            let repo_slug = parse_repo_slug(&execution.repo_remote_url);
            let branch_ok = match repo_slug {
                Ok(ref slug) => {
                    match pr_number_from_url(&staged_url) {
                        Some(pr_num) => {
                            match self.branch_verifier.fetch_pr_head_ref(slug, pr_num).await {
                                Ok(ref head_ref) if head_ref == &expected_branch => true,
                                Ok(head_ref) => {
                                    tracing::warn!(
                                        execution_id,
                                        staged_pr_url = %staged_url,
                                        staged_pr_branch = %head_ref,
                                        %expected_branch,
                                        "pr_recheck_staged_branch_mismatch: staged PR branch does not match expected; dropping staged URL",
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
        if execution.status != "waiting_human" {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                "pr-recheck: skipping fallback — execution is not waiting_human (running-status gate)",
            );
            return StopOutcome::RunningNoStagedPr;
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

        let expected_branch =
            expected_branch_name(execution.worker_branch_prefix.as_deref(), &execution.id);
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
            candidate.worker_branch_prefix.as_deref(),
            &candidate.execution_id,
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
        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            None,
            target,
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
        if let Some(lease_id) = completion.released_lease_id.as_deref() {
            if let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id,
                    source,
                    lease_id,
                    ?err,
                    "pr completion: cube release failed"
                );
            }
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
                &completion.execution.status,
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
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item {
            if task.kind == "design" {
                if let Some(ref project_id) = task.project_id {
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
                }
            }
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

    /// Primary-path handler for a `conflict_resolution` execution whose
    /// `PostToolUse` events staged at least one resolution signal
    /// (force-push or PR comment). Transitions the parent chore from
    /// `blocked` → `in_review` and marks the `conflict_resolutions`
    /// attempt `succeeded` without waiting for the merge-poller sweep.
    ///
    /// Best-effort: every step is fallible. Failures are logged and the
    /// caller falls through to `finalize_conflict_resolution_attempt`,
    /// which either marks the attempt `failed` (worker truly didn't push)
    /// or stays quiet (attempt is terminal from this call).
    async fn finalize_via_resolution_signal(
        &self,
        execution: &crate::work::WorkExecution,
    ) {
        let attempt = match self
            .work_db
            .active_conflict_resolution_for_work_item(&execution.work_item_id)
        {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    "resolution_signal: no active attempt; cannot transition parent",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "resolution_signal: failed to look up active attempt",
                );
                return;
            }
        };

        // Transition parent chore blocked → in_review. The attempt-id guard
        // ensures we only undo our own blocked row (design Q5).
        let task_transitioned = match self
            .work_db
            .clear_chore_blocked_merge_conflict_for_attempt(
                &execution.work_item_id,
                &attempt.pr_url,
                &attempt.id,
            ) {
            Ok(Some(_)) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    attempt_id = %attempt.id,
                    "resolution_signal: parent chore transitioned blocked → in_review (primary path)",
                );
                true
            }
            Ok(None) => {
                // WHERE guard missed — chore already moved (manual override
                // or concurrent on_resolved from the poller).
                tracing::debug!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    attempt_id = %attempt.id,
                    "resolution_signal: parent chore WHERE guard missed; already transitioned",
                );
                false
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "resolution_signal: failed to clear blocked merge_conflict",
                );
                false
            }
        };

        // Mark attempt succeeded. Independent of the parent-task transition
        // per design Q5: both updates run even if the other was a no-op.
        let attempt_succeeded = match self
            .work_db
            .mark_conflict_resolution_succeeded(&attempt.id, None)
        {
            Ok(Some(_)) => {
                tracing::info!(
                    attempt_id = %attempt.id,
                    "resolution_signal: attempt marked succeeded",
                );
                true
            }
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    "resolution_signal: attempt already terminal",
                );
                false
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "resolution_signal: failed to mark attempt succeeded",
                );
                false
            }
        };

        if task_transitioned {
            self.publisher
                .publish_work_item_changed(
                    &attempt.product_id,
                    &attempt.work_item_id,
                    "merge_conflict_resolved",
                )
                .await;
        }

        if attempt_succeeded {
            self.publisher
                .publish_frontend_event_on_product(
                    &attempt.product_id,
                    FrontendEvent::ConflictResolutionSucceeded {
                        product_id: attempt.product_id.clone(),
                        work_item_id: attempt.work_item_id.clone(),
                        attempt_id: attempt.id.clone(),
                        pr_url: attempt.pr_url.clone(),
                    },
                )
                .await;
        }
    }

    /// Phase 4 #11: catch-all finaliser for `conflict_resolution`
    /// workers. Fires for every Stop event on a `conflict_resolution`
    /// execution; decides whether to mark the bound
    /// `conflict_resolutions` row `failed` with the catch-all reason
    /// (`no_push_no_stop_condition`).
    ///
    /// The rule (design Q5): if the attempt is still `running`,
    /// `head_sha_after IS NULL`, `failure_reason IS NULL`, AND the
    /// worker exited without pushing (PR not freshly bound), the
    /// engine has no signal that the worker classified its own
    /// outcome — default to failed with the catch-all reason. On
    /// `Fresh` / `Merged` outcomes the merge poller's `on_resolved`
    /// retire path will mark the attempt `succeeded` shortly; we
    /// don't pre-empt it. On `Stale` / `DetectorFailed` we stay
    /// quiet because the on-Stop probe path is already chasing the
    /// situation (probe queued, worker may push again).
    ///
    /// Idempotent — the underlying [`WorkDb::mark_conflict_resolution_failed`]
    /// WHERE-guards on `status IN ('pending', 'running')`, so a
    /// duplicate finalizer call after a terminal transition is a no-op.
    pub async fn finalize_conflict_resolution_attempt(
        &self,
        execution: &crate::work::WorkExecution,
        outcome: &StopOutcome,
    ) {
        let attempt = match self
            .work_db
            .active_conflict_resolution_for_work_item(&execution.work_item_id)
        {
            Ok(Some(attempt)) => attempt,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    "conflict-resolution finalizer: no active attempt; nothing to do",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "conflict-resolution finalizer: failed to look up active attempt",
                );
                return;
            }
        };
        // Already past the "running with no outcome" window — the
        // worker reported via mark-failed, the poller already retired
        // it, or some other path closed the row. Nothing for the
        // catch-all to do.
        if attempt.status != "running"
            || attempt.head_sha_after.is_some()
            || attempt.failure_reason.is_some()
        {
            return;
        }

        let should_mark_failed = match outcome {
            // Worker pushed (or the PR is already merged from this run).
            // The merge poller's on_resolved retire path will mark the
            // attempt `succeeded` on the next sweep.
            StopOutcome::PrDetected { .. } | StopOutcome::PrMerged { .. } => false,
            // Worker pushed something but the PR head still trails the
            // worker's local commits, or pushed an empty diff. The
            // on-Stop path has already queued a probe asking the worker
            // to fix the situation; don't pre-empt that with a failed mark.
            StopOutcome::StalePr { .. } | StopOutcome::EmptyDiffPr { .. } => false,
            // Race with an already-finalized execution (a second Stop
            // for the same worker, or finalize_run racing) or a stale
            // Stop from a superseded reused-workspace occupant. Skip.
            StopOutcome::AlreadyTerminal
            | StopOutcome::UnknownExecution
            | StopOutcome::SupersededInWorkspace => false,
            // AI #6 (incident 001): the Stop hook fired on a still-`running`
            // worker with an empty staged-URL cache. The fallback didn't
            // fire by design; the worker is alive and may still push. Do
            // not pre-empt with a `failed` mark.
            StopOutcome::RunningNoStagedPr => false,
            // AI #5 (incident 001): the human flipped the
            // `detect_pr_cold_fallback` flag OFF — the fallback was
            // intentionally suppressed. The chore stays in
            // `waiting_human` for the human to resolve; do not
            // pre-empt with a `failed` mark either.
            StopOutcome::FallbackDisabledByFlag => false,
            // The auto-nudge breaker parked the execution for a human;
            // a `failed` mark could trigger a retrigger/respawn that
            // re-enters the loop. Leave the attempt for the human.
            StopOutcome::NudgeBreakerParked { .. } => false,
            // Catch-all branches: the worker exited and we have no
            // evidence of a push.
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
            .mark_conflict_resolution_failed(&attempt.id, CONFLICT_NO_PUSH_REASON)
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    "conflict-resolution finalizer: attempt already terminal between probes",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "conflict-resolution finalizer: failed to mark attempt failed",
                );
                return;
            }
        };

        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            attempt_id = %updated.id,
            pr_url = %updated.pr_url,
            reason = CONFLICT_NO_PUSH_REASON,
            ?outcome,
            "conflict-resolution finalizer: worker exited without pushing; attempt marked failed",
        );

        self.publisher
            .publish_frontend_event_on_product(
                &updated.product_id,
                FrontendEvent::ConflictResolutionFailed {
                    product_id: updated.product_id.clone(),
                    work_item_id: updated.work_item_id.clone(),
                    attempt_id: updated.id.clone(),
                    pr_url: updated.pr_url.clone(),
                    failure_reason: CONFLICT_NO_PUSH_REASON.to_owned(),
                },
            )
            .await;
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
        self.pane_releaser.release_pane(execution_id).await;

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
            if let Ok(execution) = self.work_db.get_execution(execution_id) {
                if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    let product_id = work_item_product_id(&work_item);
                    let wid = work_item_id(&work_item);
                    self.publisher
                        .publish_work_item_changed(&product_id, &wid, "worker_force_stopped")
                        .await;
                }
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
                &execution.status,
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
                        if execution.kind == "revision_implementation" {
                            execution.pr_url.clone().filter(|u| !u.is_empty())
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
                &execution.status,
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
                // a PR).  Fall back to execution.pr_url, which is set to
                // the chain root's PR URL at dispatch time.
                crate::runner::task_bound_pr_url(&task)
                    .map(str::to_owned)
                    .or_else(|| {
                        if execution.kind == "revision_implementation" {
                            execution.pr_url.clone().filter(|u| !u.is_empty())
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
                    None if execution.kind == "revision_implementation" => {
                        match execution.pr_url.clone().filter(|u| !u.is_empty()) {
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
    /// Unexpected DB failure while recording completion.
    DbError,
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
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
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, WorkDb, WorkItem,
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
    /// `headRefName` (or error) and a fixed `headRefOid` (or error)
    /// without shelling out to `gh`.
    struct StubBranchVerifier {
        result: Result<String, String>,
        head_oid_result: Mutex<Result<String, String>>,
    }

    impl StubBranchVerifier {
        /// Verifier that always reports the given branch name. The
        /// `headRefOid` defaults to the literal string `"oid_unknown"`
        /// so tests that don't touch the SHA-delta path get a stable
        /// stand-in without having to wire one explicitly. Tests that
        /// exercise the gate call [`Self::with_head_oid`] to override.
        fn ok(branch: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Ok(branch.to_owned()),
                head_oid_result: Mutex::new(Ok("oid_unknown".to_owned())),
            })
        }

        /// Override the `headRefOid` returned by `fetch_pr_head_oid`.
        /// Used by the SHA-delta gate tests to simulate a PR whose
        /// head has (or has not) moved during the worker's run.
        async fn set_head_oid(&self, oid: Result<String, String>) {
            *self.head_oid_result.lock().await = oid;
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
        ) -> Result<CubeWorkspaceLease> {
            unreachable!("not used in completion tests")
        }
        async fn create_change(
            &self,
            _: &PathBuf,
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
    }

    #[async_trait]
    impl WorkerPaneReleaser for RecordingPaneReleaser {
        async fn release_pane(&self, run_id: &str) {
            self.calls.lock().await.push(run_id.to_owned());
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        (db, product.id, chore.id, execution.id)
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

        assert!(matches!(outcome, StopOutcome::PrDetected { .. }));
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
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
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(None, &execution_id)));

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/458"),
            "expected PrDetected with staged URL, got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "the staged-URL short-circuit must skip the detector entirely (this is the whole point — no jj log, no gh api commits/{{sha}}/pulls)",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
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
        assert!(matches!(outcome, StopOutcome::PrDetected { .. }));
        assert_eq!(
            detector.call_count(),
            1,
            "with no staged URL, the detector is the only way to bind — it must be called",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
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
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(None, &execution_id)));

        // Detector intentionally returns Err — if recheck called it,
        // recheck would surface `DetectorFailed`. With the staged
        // shortcut, recheck must succeed without ever touching the
        // detector.
        let outcome = handler.recheck_for_pr(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/458"),
            "expected PrDetected from recheck via staged URL, got {outcome:?}",
        );
        assert_eq!(
            detector.call_count(),
            0,
            "recheck must skip the detector when a staged URL is present",
        );
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
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
                    t.status, "active",
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
                    t.status, "active",
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
                assert_eq!(t.status, "active", "no PR must NOT move to in_review");
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "waiting_human");
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
                    t.status, "active",
                    "stale PR must NOT move the work item to in_review",
                );
                assert!(t.pr_url.is_none(), "stale PR must NOT stamp pr_url yet");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "waiting_human");
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

        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::PrDetected { .. }
        ));
        // A second Stop event for the same execution must NOT
        // duplicate work — release is called once, work item stays
        // pinned at `in_review`. The pane releaser is invoked again
        // here; production releasers must be idempotent on their own
        // (see `WorkerRegistry::take_slot_for_run`).
        assert_eq!(
            handler.on_stop(&execution_id).await,
            StopOutcome::AlreadyTerminal,
        );
        assert_eq!(cube.release_calls.lock().await.len(), 1);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
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
                assert_eq!(t.status, "done", "merged-at-stop must skip in_review");
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/foo/bar/pull/42"),
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
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
                assert_eq!(t.status, "active");
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
    /// engine state after Phase 3 wiring spawns a resolution worker.
    fn conflict_fixture(
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
                name: "Resolve conflict".into(),
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
        let pr_url = "https://github.com/spinyfin/mono/pull/77";
        db.update_work_item(
            &chore.id,
            crate::work::WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..crate::work::WorkItemPatch::default()
            },
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&chore.id, pr_url).unwrap();
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr_url.into(),
                pr_number: 77,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base".into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
            .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "conflict_resolution".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
    async fn conflict_resolution_worker_exits_without_push_marks_attempt_failed() {
        // Worker bound to a conflict_resolutions row exits with no PR
        // (the resolver gave up without pushing). The completion path's
        // catch-all must flip the attempt to `failed` with
        // `no_push_no_stop_condition` and broadcast the typed event.
        let workspace = tempdir().unwrap();
        let (db, product_id, _chore_id, execution_id, attempt_id) =
            conflict_fixture(workspace.path());
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

        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(attempt.failure_reason.as_deref(), Some(CONFLICT_NO_PUSH_REASON));
        assert!(attempt.finished_at.is_some());

        let typed = publisher.typed_events.lock().await.clone();
        let failed_event = typed.iter().find(|(pid, ev)| {
            pid == &product_id
                && matches!(
                    ev,
                    boss_protocol::FrontendEvent::ConflictResolutionFailed {
                        attempt_id: a,
                        failure_reason,
                        ..
                    } if a == &attempt_id && failure_reason == CONFLICT_NO_PUSH_REASON
                )
        });
        assert!(
            failed_event.is_some(),
            "expected ConflictResolutionFailed event for {attempt_id}, got {typed:?}",
        );
    }

    #[tokio::test]
    async fn conflict_resolution_worker_pushed_does_not_mark_attempt_failed() {
        // Worker pushed (PrStatus::Fresh) — the merge poller's
        // `on_resolved` will mark the attempt `succeeded` on the next
        // sweep. The completion finalizer must NOT pre-empt that with
        // a `failed` mark.
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id, attempt_id) =
            conflict_fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/77"));
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

        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "running",
            "fresh-PR finalization must leave the attempt for the poller",
        );
        assert!(attempt.failure_reason.is_none());
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().all(|(_, ev)| !matches!(
                ev,
                boss_protocol::FrontendEvent::ConflictResolutionFailed { .. }
            )),
            "no Failed event must fire when the worker pushed",
        );
    }

    #[tokio::test]
    async fn conflict_resolution_worker_with_mark_failed_already_set_is_skipped() {
        // Worker called `boss engine conflicts mark-failed` first.
        // The catch-all finalizer must observe the existing
        // `failure_reason` and NOT overwrite with the catch-all.
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id, attempt_id) =
            conflict_fixture(workspace.path());
        db.mark_conflict_resolution_failed(&attempt_id, "obsolescence_suspected")
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
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(
            attempt.failure_reason.as_deref(),
            Some("obsolescence_suspected"),
            "catch-all must not overwrite an existing failure_reason",
        );
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().all(|(_, ev)| !matches!(
                ev,
                boss_protocol::FrontendEvent::ConflictResolutionFailed { .. }
            )),
            "Failed event must not be re-broadcast by the catch-all",
        );
    }

    /// Stand up a fixture for the CI-remediation completion path:
    /// chore in `blocked: ci_failure` with a `ci_remediations` row in
    /// `status='running'` and a `kind='ci_remediation'` execution row
    /// bound to the same work item. Mirrors [`conflict_fixture`] but
    /// for Phase 10 #33.
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "ci_remediation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
    async fn ci_remediation_worker_exits_without_push_marks_attempt_failed() {
        // Phase 10 #33: a `ci_remediation` worker bound to a running
        // attempt exits with no PR detected (`StopOutcome::AwaitingInput`).
        // The completion-path catch-all must flip the attempt to
        // `failed` with the documented reason and emit the typed event.
        let workspace = tempdir().unwrap();
        let (db, product_id, _chore_id, execution_id, attempt_id) =
            ci_remediation_fixture(workspace.path());
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
        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::AwaitingInput);

        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(attempt.failure_reason.as_deref(), Some(CI_NO_PUSH_REASON));
        assert!(attempt.finished_at.is_some());

        let typed = publisher.typed_events.lock().await.clone();
        let failed_event = typed.iter().find(|(pid, ev)| {
            pid == &product_id
                && matches!(
                    ev,
                    boss_protocol::FrontendEvent::CiRemediationFailed {
                        attempt_id: a,
                        failure_reason,
                        ..
                    } if a == &attempt_id && failure_reason == CI_NO_PUSH_REASON
                )
        });
        assert!(
            failed_event.is_some(),
            "expected CiRemediationFailed event for {attempt_id}, got {typed:?}",
        );
    }

    #[tokio::test]
    async fn ci_remediation_worker_pushed_does_not_mark_attempt_failed() {
        // Worker pushed (PrStatus::Fresh) — the merge poller's
        // on_ci_resolved retire path will mark the attempt `succeeded`
        // once CI goes green. The completion finalizer must NOT pre-empt
        // that with a `failed` mark.
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id, attempt_id) =
            ci_remediation_fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/88"));
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

        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "running",
            "fresh-PR finalization must leave the attempt for the poller",
        );
        assert!(attempt.failure_reason.is_none());
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().all(|(_, ev)| !matches!(
                ev,
                boss_protocol::FrontendEvent::CiRemediationFailed { .. }
            )),
            "no CiRemediationFailed event must fire when the worker pushed",
        );
    }

    #[tokio::test]
    async fn ci_remediation_worker_with_mark_failed_already_set_is_skipped() {
        // Worker called `boss engine ci mark-failed` first. The
        // completion catch-all must observe the existing failure_reason
        // and NOT overwrite with the catch-all reason.
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id, attempt_id) =
            ci_remediation_fixture(workspace.path());
        db.mark_ci_remediation_failed(&attempt_id, "triage_bailout")
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
        let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(
            attempt.failure_reason.as_deref(),
            Some("triage_bailout"),
            "catch-all must not overwrite an existing failure_reason",
        );
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().all(|(_, ev)| !matches!(
                ev,
                boss_protocol::FrontendEvent::CiRemediationFailed { .. }
            )),
            "Failed event must not be re-broadcast by the catch-all",
        );
    }

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
    async fn conflict_resolution_on_stop_uses_staged_signal_and_transitions_parent() {
        // Primary-path test: a ForcePushed signal is staged before Stop fires.
        // The on-Stop handler must transition the parent chore blocked →
        // in_review and mark the attempt succeeded — without a gh pr
        // round-trip. The PR detector is set to return None so any
        // mergeability call would leave the attempt in `running`; if the
        // assertion passes the staged-signal path (not the detector) ran.
        let workspace = tempdir().unwrap();
        let (db, product_id, chore_id, execution_id, attempt_id) =
            conflict_fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let signals = Arc::new(
            crate::resolution_signal_capture::StagedResolutionSignalCache::new(),
        );
        signals.record_signal(
            &execution_id,
            crate::resolution_signal_capture::ResolutionSignal::ForcePushed,
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        .with_staged_resolution_signals(signals);

        let _ = handler.on_stop(&execution_id).await;

        // Parent chore must be in_review with blocked columns cleared.
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review", "parent must transition to in_review");
                assert!(t.blocked_reason.is_none(), "blocked_reason must be cleared");
                assert!(t.blocked_attempt_id.is_none(), "blocked_attempt_id must be cleared");
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // Attempt must be succeeded (not left running for the poller).
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "succeeded",
            "attempt must be marked succeeded by primary path",
        );

        // ConflictResolutionSucceeded event must be published.
        let typed = publisher.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(pid, ev)| {
                pid == &product_id
                    && matches!(
                        ev,
                        boss_protocol::FrontendEvent::ConflictResolutionSucceeded {
                            attempt_id: a,
                            ..
                        } if a == &attempt_id
                    )
            }),
            "expected ConflictResolutionSucceeded for {attempt_id}; got {typed:?}",
        );

        // ConflictResolutionFailed must NOT be published.
        assert!(
            typed.iter().all(|(_, ev)| !matches!(
                ev,
                boss_protocol::FrontendEvent::ConflictResolutionFailed { .. }
            )),
            "ConflictResolutionFailed must not fire when primary path succeeds",
        );
    }

    #[tokio::test]
    async fn conflict_resolution_on_stop_with_no_staged_signal_falls_back_to_finalizer() {
        // Cold-path regression: empty staging cache — the catch-all
        // finalizer must run and mark the attempt failed (worker exited
        // without pushing). This is the pre-existing behaviour; the new
        // primary-path code must not change it.
        let workspace = tempdir().unwrap();
        let (db, _product_id, _chore_id, execution_id, attempt_id) =
            conflict_fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // No signals staged — uses the default empty cache from `new`.
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

        // Attempt must be failed by the catch-all.
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(
            attempt.failure_reason.as_deref(),
            Some(CONFLICT_NO_PUSH_REASON),
        );
    }

    /// Branch-keyed detection (AI #6, incident 001): the detector
    /// queries `gh pr list --head <branch>` and trusts the branch as
    /// the unique attribution signal. The squash-merge-on-`main`
    /// misbind from PR #379 (where `@-` resolved to the merge commit
    /// of an unrelated PR) is now structurally impossible: a sibling
    /// worker's bookmark cannot share this execution's branch name
    /// because the engine derives it from `execution_id`. The
    /// classifier therefore needs no head_sha gate.
    #[test]
    fn classify_pr_merged_is_merged() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/42".into(),
            state: "MERGED".into(),
            merged_at: Some("2026-05-12T04:00:00Z".into()),
            changed_files: 5,
            additions: 12,
            deletions: 4,
        };
        assert_eq!(
            classify_pr(pr),
            PrStatus::Merged {
                url: "https://github.com/foo/bar/pull/42".into(),
            },
        );
    }

    #[test]
    fn classify_pr_closed_unmerged_is_closed() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/100".into(),
            state: "CLOSED".into(),
            merged_at: None,
            changed_files: 2,
            additions: 1,
            deletions: 1,
        };
        assert_eq!(
            classify_pr(pr),
            PrStatus::Closed {
                url: "https://github.com/foo/bar/pull/100".into(),
            },
        );
    }

    /// All three diff-stat fields zero — tentative EmptyDiff. The
    /// secondary verification call in `detect_pr` confirms before
    /// surfacing this to callers.
    #[test]
    fn classify_pr_returns_empty_diff_when_all_diff_stats_are_zero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/55".into(),
            state: "OPEN".into(),
            merged_at: None,
            changed_files: 0,
            additions: 0,
            deletions: 0,
        };
        assert_eq!(
            classify_pr(pr),
            PrStatus::EmptyDiff {
                url: "https://github.com/foo/bar/pull/55".into(),
            },
        );
    }

    /// Regression: `changed_files == 0` must NOT produce `EmptyDiff`
    /// when `additions` or `deletions` are non-zero. GitHub computes
    /// `changed_files` asynchronously and can return 0 for a
    /// freshly-pushed branch while `additions` / `deletions` are
    /// already populated. Before PR #446 the engine injected a bogus
    /// "your diff is empty" directive into the worker pane on every
    /// Stop event in this case.
    #[test]
    fn classify_pr_returns_fresh_when_changed_files_zero_but_additions_nonzero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/446".into(),
            state: "OPEN".into(),
            merged_at: None,
            changed_files: 0,
            additions: 1,
            deletions: 1,
        };
        assert_eq!(
            classify_pr(pr),
            PrStatus::Fresh {
                url: "https://github.com/foo/bar/pull/446".into(),
            },
        );
    }

    #[test]
    fn classify_pr_returns_fresh_when_changed_files_nonzero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/56".into(),
            state: "OPEN".into(),
            merged_at: None,
            changed_files: 1,
            additions: 0,
            deletions: 0,
        };
        assert_eq!(
            classify_pr(pr),
            PrStatus::Fresh {
                url: "https://github.com/foo/bar/pull/56".into(),
            },
        );
    }

    /// AI #6 regression: the branch name passed to `detect_pr` must be
    /// derived deterministically from `execution_id` — and two
    /// different executions must derive two different branches. This
    /// is the structural property that makes the cross-workspace
    /// fan-out from incident 001 impossible: a sibling worker in
    /// another cube workspace has a different execution ID, therefore
    /// pushes to a different branch, therefore cannot be matched by
    /// this execution's `gh pr list --head <branch>` query.
    #[test]
    fn expected_branch_name_is_deterministic_and_unique_per_execution() {
        let a = expected_branch_name(None, "exec_18af6057fe1514f8_3");
        let b = expected_branch_name(None, "exec_18af6057fe1514f8_3");
        assert_eq!(a, b, "branch name must be deterministic for a given execution id");
        let other = expected_branch_name(None, "exec_999999999999_4");
        assert_ne!(
            a, other,
            "two distinct execution ids must produce distinct branch names — \
             this is the load-bearing structural property of AI #6",
        );
        // The execution id must be recoverable from the branch (the
        // engine derives the name; the detector re-derives it from
        // state.db). Easiest property to assert: the id is embedded.
        assert!(
            a.contains("exec_18af6057fe1514f8_3"),
            "branch name must contain the execution id so the detector can re-derive it: {a}",
        );
    }

    #[test]
    fn expected_branch_name_default_prefix_is_boss() {
        assert_eq!(
            expected_branch_name(None, "exec_18af6057fe1514f8_3"),
            "boss/exec_18af6057fe1514f8_3",
            "no configured prefix must preserve the historical boss/ shape",
        );
    }

    #[test]
    fn expected_branch_name_honours_configured_prefix_keeping_exec_suffix() {
        let branch = expected_branch_name(Some("bduff/"), "exec_18af6057fe1514f8_3");
        assert_eq!(branch, "bduff/exec_18af6057fe1514f8_3");
        // The exec id — the stable identifier every subsystem keys off —
        // must remain embedded so the detector can still re-derive it
        // regardless of the configured prefix.
        assert!(
            branch.contains("exec_18af6057fe1514f8_3"),
            "configured prefix must not displace the exec_<id> suffix: {branch}",
        );
    }

    /// AI #6 cross-workspace regression: two concurrent workers in
    /// different cube workspaces — Alice with one execution id, Bob
    /// with another — each fire `on_stop` with an empty staged-URL
    /// cache. Each handler must call `detect_pr` with its OWN
    /// execution's branch name. Pre-fix the detector used a workspace-
    /// scoped jj revset and would routinely return Bob's PR for
    /// Alice's Stop event, fan-binding the wrong URL onto the wrong
    /// chore (the 2026-05-14 fan-out).
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

        let alice_url = match alice_outcome {
            StopOutcome::PrDetected { pr_url } => pr_url,
            other => panic!("alice expected PrDetected, got {other:?}"),
        };
        let bob_url = match bob_outcome {
            StopOutcome::PrDetected { pr_url } => pr_url,
            other => panic!("bob expected PrDetected, got {other:?}"),
        };
        assert_ne!(
            alice_url, bob_url,
            "two concurrent workers in different workspaces must bind to different PRs — \
             the fan-out bug from incident 001 was exactly the case where they got the same one",
        );
        assert!(
            alice_url.contains(&expected_branch_name(None, &alice_exec)),
            "alice's bound URL must derive from her own execution id, got {alice_url}",
        );
        assert!(
            bob_url.contains(&expected_branch_name(None, &bob_exec)),
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
        let (_e, run) = db
            .start_execution_run(&exec.id, "worker", "mono", lease, workspace_id, workspace_path)
            .unwrap();
        db.finish_execution_run(
            &exec.id,
            &run.id,
            "waiting_human",
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
                t.status, "in_review",
                "the stale occupant's task must not transition on a leaked Stop",
            ),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "a leaked stale Stop must not release any cube lease",
        );

        // The live (newest) execution's own Stop still finalizes it.
        assert!(
            matches!(handler.on_stop(&live_exec).await, StopOutcome::PrDetected { .. }),
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
        assert_eq!(execution.status, "running");
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
                assert_eq!(t.status, "active");
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
        assert!(matches!(outcome, StopOutcome::PrDetected { .. }));
        assert_eq!(detector.call_count(), 1);
        let calls = detector.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].expected_branch,
            expected_branch_name(None, &execution_id),
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
                    t.status, "active",
                    "empty-diff PR must NOT move the work item to in_review",
                );
                assert!(t.pr_url.is_none(), "empty-diff PR must NOT stamp pr_url");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "waiting_human");
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
                    t.status, "active",
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
            exec_after.status, "waiting_human",
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
        match outcome {
            StopOutcome::PrDetected { pr_url } => {
                assert_eq!(
                    pr_url, workers_actual_pr,
                    "must bind to the worker-created PR, not the description-mentioned one",
                );
            }
            other => panic!("expected PrDetected, got {other:?}"),
        }

        let item = db.get_work_item(&chore.id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
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

        // Chore advanced to in_review with the PR url stamped.
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert_eq!(t.pr_url.as_deref(), Some(workers_pr));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        // Execution finalised — lease released, pane torn down.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
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
                assert_eq!(t.status, "active");
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
            .create_execution(crate::work::CreateExecutionInput {
                work_item_id: chore2.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
            .create_execution(crate::work::CreateExecutionInput {
                work_item_id: chore3.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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
                "waiting_human",
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
                    assert_eq!(t.status, "active");
                    assert!(t.pr_url.is_none());
                }
                other => panic!("expected chore, got {other:?}"),
            }
        }
        for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
            let execution = db.get_execution(execution_id).unwrap();
            assert_eq!(
                execution.status, "waiting_human",
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
        for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
            let item = db.get_work_item(chore_id).unwrap();
            match item {
                WorkItem::Chore(t) => {
                    assert_eq!(
                        t.status, "in_review",
                        "chore {chore_id} must transition to in_review on the recovery pass",
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
                execution.status, "completed",
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
                assert_eq!(t.status, "active");
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            execution.status, "waiting_human",
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
        assert!(
            matches!(outcome, StopOutcome::PrDetected { .. }),
            "default-ON must still let `detect_pr` fire; got {outcome:?}",
        );
        assert_eq!(detector.call_count(), 1);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
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
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/606"),
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
                assert_eq!(t.status, "in_review");
                assert_eq!(t.pr_url.as_deref(), Some(pr_url));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
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
                assert_eq!(t.status, "active");
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
        assert!(matches!(outcome, StopOutcome::PrDetected { .. }));
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
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
            WorkItem::Chore(t) => assert_eq!(t.status, "active"),
            other => panic!("expected chore, got {other:?}"),
        }
        let items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
            "parking must file an attention item",
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
        assert!(
            matches!(final_outcome, StopOutcome::PrDetected { .. }),
            "the worker's real PR must finalize; got {final_outcome:?}",
        );
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
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
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
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
            "waiting_human",
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
        assert_eq!(task.status, "in_review");
        assert_eq!(
            task.pr_url.as_deref(),
            Some("https://github.com/spinyfin/mono/pull/42")
        );
        // Execution itself stays abandoned — recheck_for_pr_late does not
        // touch the execution row.
        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, "abandoned");
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
        assert_eq!(task.status, "active");
        assert!(task.pr_url.is_none());
    }
}
