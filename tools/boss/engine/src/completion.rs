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
//! The completion signal we listen for is the worker's `Stop` hook
//! event. On every Stop, we resolve the worker's local commit shas
//! via `jj log` (cube workspaces are non-colocated, so a top-level
//! `git` invocation has no repo to point at — we cannot rely on
//! `gh pr view` to figure out the branch), then ask the GitHub API
//! `repos/{owner}/{repo}/commits/{sha}/pulls` whether any PR
//! contains those commits. If a fresh open PR exists, the work item
//! moves to `in_review`, the execution finalises (status `completed`,
//! lease cleared, finished_at stamped), and the cube workspace is
//! released so the next dispatch can take it over. If the PR is
//! already merged by the time the Stop fires, the work item moves
//! straight to `done`. If there is no PR, we surface an
//! "awaiting input" signal on the execution topic so the coordinator
//! / pane indicator can show the worker is idle without moving the
//! work item to review.
//!
//! Merges that happen *after* the worker exited are detected by a
//! periodic poller wired in `app.rs`, which calls
//! [`WorkDb::mark_chore_pr_merged`] for any chore in `in_review`
//! whose `pr_url` is now in a merged GitHub state.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::work::{WorkDb, WorkItem, WorkerPrCompletionTarget};

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
            | PrStatus::Merged { url }
            | PrStatus::Closed { url } => Some(url),
        }
    }
}

/// Probes a workspace for any PR associated with its local commits
/// and reports whether the PR is open / merged / stale / absent.
///
/// `repo_remote_url` is the product's `git@github.com:owner/repo.git`
/// (or `https://...`) URL — the detector parses it into an
/// `owner/repo` slug to query the GitHub API directly. Cube
/// workspaces are non-colocated, so passing a workspace path to
/// `gh pr view` doesn't work (no top-level `.git`); the detector
/// must reach the API some other way, and the slug is the most
/// reliable signal we have.
#[async_trait]
pub trait PrDetector: Send + Sync {
    /// Returns the workspace's PR status. Implementations must treat
    /// "no PR" as `Ok(PrStatus::None)` to keep the caller's
    /// idle-vs-completed logic clean. Errors are reserved for tool
    /// failures (jj missing, `gh` auth broken, etc.).
    async fn detect_pr(
        &self,
        workspace_path: &Path,
        repo_remote_url: &str,
    ) -> Result<PrStatus>;
}

/// `PrDetector` that shells out to `jj log` plus `gh api`. We can't
/// use `gh pr view` from the workspace because cube workspaces are
/// non-colocated jj checkouts (no `.git` at the workspace root, so
/// `gh`'s implicit `git rev-parse --abbrev-ref HEAD` fails). Instead:
///
/// 1. Ask `jj log` for the worker's working-copy commit and its
///    parent (covers both "@ has the work" and "squashed into @-,
///    @ is empty" patterns).
/// 2. Parse `repo_remote_url` into an `owner/repo` slug.
/// 3. Hit `repos/{owner}/{repo}/commits/{sha}/pulls` for each
///    candidate sha — GitHub returns the PR that contains the
///    commit (open PRs while the branch lives, merged PRs when the
///    commit landed in `main`).
/// 4. Map the response (state, merged_at, head_sha) onto
///    [`PrStatus`].
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
        workspace_path: &Path,
        repo_remote_url: &str,
    ) -> Result<PrStatus> {
        let repo_slug = parse_repo_slug(repo_remote_url).with_context(|| {
            format!("failed to parse repo slug from `{repo_remote_url}`")
        })?;
        let mut candidates = jj_candidate_commit_shas(workspace_path).await?;
        // `@` and `@-` resolve to the same commit on a fresh,
        // single-commit workspace; skip the duplicate API call.
        candidates.dedup();
        if candidates.is_empty() {
            // No commits to search — workspace is empty / brand new.
            return Ok(PrStatus::None);
        }

        // Walk the candidates newest-first (`@` before `@-`). The
        // first sha that resolves to a PR wins. We hold onto the
        // most recent transient `gh` error so detector failures still
        // surface if every candidate failed.
        let mut last_err: Option<anyhow::Error> = None;
        for sha in &candidates {
            match query_pr_for_commit(&repo_slug, sha).await {
                Ok(Some(api_pr)) => return Ok(classify_pr(api_pr, &candidates)),
                Ok(None) => continue,
                Err(err) => {
                    tracing::debug!(
                        sha,
                        repo = %repo_slug,
                        ?err,
                        "gh api commits/{sha}/pulls failed; trying next candidate",
                    );
                    last_err = Some(err);
                }
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(PrStatus::None)
    }
}

/// Single PR row returned from `gh api repos/{owner}/{repo}/commits/{sha}/pulls`.
#[derive(Debug, Clone)]
struct ApiPr {
    url: String,
    state: String,
    merged_at: Option<String>,
    head_sha: String,
}

fn classify_pr(pr: ApiPr, local_shas: &[String]) -> PrStatus {
    if pr.merged_at.is_some() {
        return PrStatus::Merged { url: pr.url };
    }
    if pr.state.eq_ignore_ascii_case("closed") {
        return PrStatus::Closed { url: pr.url };
    }
    let head_match = local_shas
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&pr.head_sha));
    if head_match {
        PrStatus::Fresh { url: pr.url }
    } else {
        PrStatus::Stale {
            url: pr.url,
            reason: format!(
                "local commits do not match PR head {pr_head}",
                pr_head = short_sha(&pr.head_sha),
            ),
        }
    }
}

/// `jj log -r '@ | @-' --no-graph -T 'commit_id ++ "\n"'` — read the
/// worker's working-copy commit and its parent. The two-rev fallback
/// covers the two normal end-states for a worker run:
///   - they did `jj squash` and the work lives on `@-` (with `@`
///     left as an empty change), or
///   - they edited `@` directly so the work lives there.
/// Either way, querying both shas catches the PR.
async fn jj_candidate_commit_shas(workspace_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "--ignore-working-copy",
            "-r",
            "@ | @-",
            "-T",
            r#"commit_id ++ "\n""#,
        ])
        .current_dir(workspace_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn `jj log` in {}",
                workspace_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "`jj log` failed in {}: {}",
            workspace_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// `gh api repos/{owner}/{repo}/commits/{sha}/pulls` — return the
/// first PR associated with `sha`, or `Ok(None)` if there isn't one.
/// `Err(_)` is reserved for tool / network failures.
async fn query_pr_for_commit(repo_slug: &str, sha: &str) -> Result<Option<ApiPr>> {
    let endpoint = format!("repos/{repo_slug}/commits/{sha}/pulls");
    let output = Command::new("gh")
        .args([
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            r#"first | select(.) | [(.html_url // ""), (.state // ""), (.merged_at // ""), (.head.sha // "")] | @tsv"#,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh api {endpoint}`"))?;
    if !output.status.success() {
        let stderr_lower = String::from_utf8_lossy(&output.stderr).to_lowercase();
        // 422 is what GitHub returns for "no commit found" on this
        // endpoint when the sha isn't in the repo (e.g. the worker
        // never pushed). Treat as "no PR" rather than an error so
        // the caller's idle-vs-completed branch stays clean.
        if stderr_lower.contains("404")
            || stderr_lower.contains("422")
            || stderr_lower.contains("not found")
        {
            return Ok(None);
        }
        return Err(anyhow!(
            "`gh api {endpoint}` failed: {}",
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
    let head_sha = parts.next().unwrap_or("").trim().to_owned();
    if url.is_empty() {
        return Ok(None);
    }
    let merged_at = if merged_at_raw.is_empty() || merged_at_raw.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(merged_at_raw.to_owned())
    };
    Ok(Some(ApiPr {
        url,
        state,
        merged_at,
        head_sha,
    }))
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

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
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
        Self {
            work_db,
            pr_detector,
            cube_client,
            publisher,
            pane_releaser,
            probe_queuer,
        }
    }

    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
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

        let workspace_path = match execution.workspace_path.as_deref() {
            Some(path) => PathBuf::from(path),
            None => {
                tracing::warn!(
                    execution_id,
                    "stop event: execution has no workspace_path — cannot detect PR"
                );
                return StopOutcome::NoWorkspace;
            }
        };

        let pr_status = match self
            .pr_detector
            .detect_pr(&workspace_path, &execution.repo_remote_url)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    ?err,
                    "stop event: PR detection failed; surfacing as awaiting input"
                );
                self.publish_awaiting_input(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_DETECTOR_FAILURE);
                return StopOutcome::DetectorFailed;
            }
        };

        let (pr_url, target) = match pr_status {
            PrStatus::None | PrStatus::Closed { .. } => {
                tracing::info!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    "stop event: worker idle without an active PR — probing to push and open one"
                );
                self.publish_awaiting_pr(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_NO_PR);
                return StopOutcome::AwaitingInput;
            }
            PrStatus::Stale { url, reason } => {
                tracing::info!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    pr_url = %url,
                    %reason,
                    "stop event: PR exists but local commits are unpushed — probing to push"
                );
                self.publish_awaiting_pr(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_STALE_PR);
                return StopOutcome::StalePr { pr_url: url, reason };
            }
            PrStatus::Fresh { url } => (url, WorkerPrCompletionTarget::InReview),
            PrStatus::Merged { url } => (url, WorkerPrCompletionTarget::Done),
        };
        let merged = matches!(target, WorkerPrCompletionTarget::Done);

        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            None,
            target,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => {
                // Race: another Stop event finalised the execution
                // between our status check and the DB update.
                return StopOutcome::AlreadyTerminal;
            }
            Err(err) => {
                tracing::error!(
                    execution_id,
                    ?err,
                    "stop event: failed to record PR completion"
                );
                return StopOutcome::DbError;
            }
        };

        if let Some(lease_id) = completion.released_lease_id.as_deref() {
            if let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id,
                    lease_id,
                    ?err,
                    "stop event: PR completion recorded but cube release failed"
                );
            }
        }

        // Tear down the libghostty pane that was hosting the worker.
        // Idempotent on the registry side, so a later manual stop /
        // chore-done update for the same run is a no-op.
        self.pane_releaser.release_pane(execution_id).await;

        let product_id = work_item_product_id(&completion.work_item);
        let work_item_id = work_item_id(&completion.work_item);
        let publish_reason = if merged {
            "worker_pr_merged"
        } else {
            "worker_pr_completed"
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
        if merged {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                "stop event: worker PR already merged; moved work item to done"
            );
            StopOutcome::PrMerged { pr_url }
        } else {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                "stop event: worker PR detected; moved work item to in_review"
            );
            StopOutcome::PrDetected { pr_url }
        }
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

    async fn publish_awaiting_input(&self, execution: &crate::work::WorkExecution) {
        // Status string mirrors what the execution actually is in DB,
        // but the reason is what carries the "awaiting input" signal
        // — frontends can surface that as the idle/awaiting indicator
        // on the worker pane.
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                &execution.status,
                "worker_awaiting_input",
            )
            .await;
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
}

/// Probe text dispatched when a worker stops without producing any PR
/// for its branch. Phrased so a worker that already finished the work
/// will simply push and open one, but a worker that's blocked has an
/// out to explain itself rather than churning.
pub const PROBE_NO_PR: &str = "You stopped without producing a PR for this work. \
If the work is complete, push your branch and open the PR with `gh pr create`. \
If you're blocked, explain what you need.";

/// Probe text dispatched when a PR exists but the worker has local
/// commits that haven't been pushed yet — the PR is stale.
pub const PROBE_STALE_PR: &str = "A PR exists for this branch, but your local commits \
are ahead of the PR's head. Push the new commits (`jj git push -b <bookmark>`) \
so the PR reflects your latest work, or explain why the local commits should not \
be pushed.";

/// Probe text dispatched when the PR detector itself errored. We don't
/// know whether a PR exists, so ask the worker to confirm.
pub const PROBE_DETECTOR_FAILURE: &str = "I couldn't determine whether you've opened \
a PR for this branch (the `gh` query failed). If a PR exists, paste its URL on its \
own line. If not, push your branch and open one with `gh pr create`. If you're \
blocked, explain what you need.";

/// What happened during a stop event handler invocation. The runtime
/// only logs this; tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Stop arrived for a run id that doesn't map to a known execution
    /// (e.g., test infra, agent runs).
    UnknownExecution,
    /// Execution was already in a terminal status — no transition.
    AlreadyTerminal,
    /// Execution had no workspace_path recorded.
    NoWorkspace,
    /// `gh` failed with a non-"no-PR" error; surfaced as awaiting input.
    DetectorFailed,
    /// No PR yet — worker is idle awaiting input.
    AwaitingInput,
    /// PR detected; work item moved to `in_review` and execution finalised.
    PrDetected { pr_url: String },
    /// PR detected and already merged at Stop time; work item moved
    /// straight to `done` and execution finalised.
    PrMerged { pr_url: String },
    /// PR exists but local commits are ahead of its head sha. The
    /// worker is probed to push the missing commits; the work item
    /// stays in its current state until the next Stop reports a fresh PR.
    StalePr { pr_url: String, reason: String },
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
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, WorkDb, WorkItem,
    };

    struct StubPrDetector {
        result: Mutex<Result<PrStatus, String>>,
    }

    impl StubPrDetector {
        fn ok(value: Option<&str>) -> Arc<Self> {
            let status = match value {
                Some(url) => PrStatus::Fresh { url: url.to_owned() },
                None => PrStatus::None,
            };
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
            })
        }

        fn ok_status(status: PrStatus) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
            })
        }

        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Err(message.to_owned())),
            })
        }
    }

    #[async_trait]
    impl PrDetector for StubPrDetector {
        async fn detect_pr(
            &self,
            _workspace_path: &Path,
            _repo_remote_url: &str,
        ) -> Result<PrStatus> {
            let guard = self.result.lock().await;
            match &*guard {
                Ok(value) => Ok(value.clone()),
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Detect worker stop".into(),
                description: None,
                autostart: true,
                priority: None,
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
    async fn detector_failure_is_treated_as_awaiting_input() {
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
        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_input"),
            "detector errors must surface as awaiting_input, got {events:?}",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "detector failures must still probe the worker");
        assert_eq!(queued[0].1, PROBE_DETECTOR_FAILURE);
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
}
