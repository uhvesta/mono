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

use boss_protocol::FrontendEvent;

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::work::{WorkDb, WorkItem, WorkerPrCompletionTarget};

/// Catch-all `failure_reason` stamped on a `conflict_resolutions` row
/// when the bound worker exits without pushing and without otherwise
/// classifying the failure via `boss engine conflicts mark-failed`
/// (design Q5 / Phase 4 #11). The activity-feed surface renders it
/// loudly so the user knows the engine gave up rather than churning.
pub const CONFLICT_NO_PUSH_REASON: &str = "no_push_no_stop_condition";

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
    ///
    /// `dispatch_started_at` is the execution row's `started_at` — a unix
    /// epoch seconds string (the engine's `now_string()` format, e.g.
    /// `"1778714114"`) — and gates the candidate expansion against stale
    /// bookmarks accumulated from prior tasks in this cube workspace.
    /// `None` keeps the legacy `@ | @-`-only behaviour for executions
    /// that have not yet started — those shouldn't reach a Stop hook in
    /// practice, but the parameter is optional rather than required so
    /// callers handle the missing case explicitly instead of trusting the row.
    async fn detect_pr(
        &self,
        workspace_path: &Path,
        repo_remote_url: &str,
        dispatch_started_at: Option<&str>,
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
        dispatch_started_at: Option<&str>,
    ) -> Result<PrStatus> {
        let repo_slug = parse_repo_slug(repo_remote_url).with_context(|| {
            format!("failed to parse repo slug from `{repo_remote_url}`")
        })?;
        let mut candidates =
            jj_candidate_commit_shas(workspace_path, dispatch_started_at).await?;
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
                Ok(Some(api_pr)) => {
                    let api_pr_url = api_pr.url.clone();
                    let api_pr_head = api_pr.head_sha.clone();
                    let status = classify_pr(api_pr, &candidates);
                    // When all three diff-stat fields are zero the PR is
                    // *tentatively* empty, but GitHub computes those stats
                    // asynchronously.  Run a secondary check against the
                    // full PR endpoint before surfacing EmptyDiff — a false
                    // positive here would loop the worker pane with bogus
                    // "your diff is empty" directives on every Stop event.
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
                    // Diagnostic surface for the "worker pushed a real PR
                    // but the engine can't bind it" failure mode: when we
                    // got a PR back from `gh api` but `classify_pr`
                    // rejected `head_match`, log the SHAs side-by-side so
                    // operators can see whether the worker's `@`/`@-`
                    // drifted from the PR's head (e.g., they did `jj new
                    // main` after `jj git push`). Without this, a worker
                    // stuck in `active`/`waiting_human` is invisible to
                    // log inspection — the existing `info!` in
                    // `on_stop_inner` says "PR exists but local commits
                    // are unpushed" without showing which SHAs failed.
                    if matches!(status, PrStatus::Stale { .. }) {
                        tracing::info!(
                            workspace = %workspace_path.display(),
                            repo = %repo_slug,
                            pr_url = %api_pr_url,
                            pr_head_sha = %api_pr_head,
                            local_shas = ?candidates,
                            queried_sha = %sha,
                            "pr_detect: PR found but head_sha does not match any local commit — \
                             worker's working copy likely moved after push (e.g., `jj new main`)",
                        );
                    }
                    return Ok(status);
                }
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
        // No candidate resolved to a PR. Log at debug so the next
        // recheck-loop pass leaves a breadcrumb; the merge poller calls
        // `recheck_for_pr` every 60s, and the steady-state "no PR" case
        // is normal noise.
        tracing::debug!(
            workspace = %workspace_path.display(),
            repo = %repo_slug,
            candidate_count = candidates.len(),
            "pr_detect: no PR found for any local commit; returning None",
        );
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

fn classify_pr(pr: ApiPr, local_shas: &[String]) -> PrStatus {
    // Structural safety belt against the "worker's `@-` is a recent
    // squash-merge commit on `main`" misbind. `jj_candidate_commit_shas`
    // returns both `@` and `@-`; when the worker did `jj new main` and
    // committed on `@` without pushing, `@-` is the tip of `main` —
    // which is the merge commit of whatever PR landed most recently.
    // The GitHub `commits/{sha}/pulls` endpoint then happily returns
    // that unrelated, already-merged PR. Without this gate the
    // completion handler stamps the chore's `pr_url` with it and
    // transitions to `done`, so the kanban Review column is skipped
    // and the audit trail points at an unrelated PR.
    //
    // The legitimate worker-attribution path is: the worker pushed
    // their branch (or it was pushed previously) and at least one
    // local commit sha matches the PR's `head.sha`. For unmerged PRs
    // we already required `head_match` to classify as `Fresh`; require
    // it for `Merged` and `Closed` too so a commit that the worker
    // didn't actually create the PR for can't get bound. Failing the
    // gate returns `None`, which the on-Stop handler treats as
    // "awaiting input" — the worker is probed to push and open a PR.
    let head_match = local_shas
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&pr.head_sha));

    if pr.merged_at.is_some() {
        if head_match {
            return PrStatus::Merged { url: pr.url };
        }
        return PrStatus::None;
    }
    if pr.state.eq_ignore_ascii_case("closed") {
        if head_match {
            return PrStatus::Closed { url: pr.url };
        }
        return PrStatus::None;
    }
    if head_match {
        // A PR has real changes if ANY of the three diff-stat fields is
        // positive.  `changed_files` alone is unreliable: GitHub computes
        // it asynchronously and the `commits/{sha}/pulls` endpoint can
        // return 0 for a freshly-pushed branch before the computation
        // finishes.  `additions` and `deletions` are populated by the same
        // pipeline but are often available sooner.  If ALL three are zero
        // the PR is tentatively empty; `detect_pr` runs a secondary
        // verification call against the full PR endpoint before surfacing
        // `EmptyDiff` to callers.
        let has_changes =
            pr.changed_files > 0 || pr.additions > 0 || pr.deletions > 0;
        if has_changes {
            PrStatus::Fresh { url: pr.url }
        } else {
            PrStatus::EmptyDiff { url: pr.url }
        }
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

/// Read candidate commit shas for PR detection.
///
/// Always includes `@ | @-` — the worker's working-copy commit and its
/// parent. The two-rev fallback covers the two normal end-states for
/// a worker run:
///   - they did `jj squash` and the work lives on `@-` (with `@`
///     left as an empty change), or
///   - they edited `@` directly so the work lives there.
///
/// When `dispatch_started_at` is `Some`, the candidate set is extended
/// with the tip of every bookmark whose tip commit was committed after
/// that timestamp. This closes the bug that left Worf / Crusher / Troi
/// (2026-05-13 dispatch wave, five-worker repro) stuck in `active` /
/// `waiting_human`: those workers had pushed a real PR with a bookmark
/// pointing at their work, then done `jj new main` afterwards, so
/// `@` / `@-` no longer reached the pushed commit and the GitHub API
/// query returned an unrelated already-merged PR off `main`'s tip
/// which `classify_pr` correctly rejected as `Stale`. With the
/// bookmark's tip in the candidate set, the worker's pushed sha is
/// queried and `classify_pr` accepts it as `Fresh`.
///
/// The `committer_date(after:)` gate scopes the bookmark expansion to
/// this dispatch's run window. Cube workspaces accumulate 50+ stale
/// bookmarks from prior tasks (each was pushed at some point, so they
/// have remote-tracking entries too), and including them naively
/// would misbind the chore to whichever prior PR happened to be
/// queried first. Filtering by `started_at` keeps only bookmarks the
/// current worker actually moved.
async fn jj_candidate_commit_shas(
    workspace_path: &Path,
    dispatch_started_at: Option<&str>,
) -> Result<Vec<String>> {
    let revset = build_candidate_revset(dispatch_started_at);
    let output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "--ignore-working-copy",
            "-r",
            &revset,
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
    let mut shas: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    // De-duplicate while preserving newest-first order: `@` always
    // appears before `@-`, and any bookmark tip that coincides with
    // either should not be queried twice. A small `seen` set is
    // cheaper than a sort for the typical candidate count (< 10).
    let mut seen = std::collections::HashSet::new();
    shas.retain(|sha| seen.insert(sha.clone()));
    Ok(shas)
}

/// Convert a unix epoch seconds string (the engine's `now_string()` format)
/// to an ISO 8601 / RFC 3339 string jj can parse. Returns `None` if the
/// input is not a valid non-negative integer or is out of range.
fn epoch_seconds_to_iso8601(s: &str) -> Option<String> {
    let secs: i64 = s.trim().parse().ok()?;
    if secs < 0 {
        return None;
    }
    let days = secs / 86400;
    let rem = secs % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    // Civil-from-days: Howard Hinnant's algorithm (public domain).
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hh, mm, ss
    ))
}

/// Compose the revset for `jj_candidate_commit_shas`. Split out so the
/// committer-date gate is tested independently of the `jj log`
/// invocation.
///
/// `dispatch_started_at` is the execution row's `started_at` — a unix
/// epoch seconds string (the engine's `now_string()` format). It is
/// converted to ISO 8601 before embedding in the revset so that jj can
/// parse it. If the value cannot be converted (non-numeric or negative),
/// the function fails closed by returning the legacy `@ | @-` revset
/// rather than producing an invalid query that would fail the whole
/// detection pass.
fn build_candidate_revset(dispatch_started_at: Option<&str>) -> String {
    match dispatch_started_at.map(str::trim).filter(|s| !s.is_empty()) {
        Some(started) => {
            let Some(iso) = epoch_seconds_to_iso8601(started) else {
                return "@ | @-".to_owned();
            };
            format!(
                r#"@ | @- | (bookmarks() & committer_date(after:"{iso}"))"#,
            )
        }
        None => "@ | @-".to_owned(),
    }
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
            r#"first | select(.) | [(.html_url // ""), (.state // ""), (.merged_at // ""), (.head.sha // ""), ((.changed_files // 0) | tostring), ((.additions // 0) | tostring), ((.deletions // 0) | tostring)] | @tsv"#,
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
        head_sha,
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
            staged_pr_urls: Arc::new(crate::pr_url_capture::StagedPrUrlCache::new()),
        }
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

    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        let outcome = self.on_stop_inner(execution_id).await;
        // Phase 4 #11: for `conflict_resolution` executions, run the
        // catch-all attempt finalizer regardless of how the inner path
        // resolved. The finalizer decides whether to mark the bound
        // `conflict_resolutions` row `failed` based on whether the
        // worker pushed (see [`Self::finalize_conflict_resolution_attempt`]).
        if let Ok(execution) = self.work_db.get_execution(execution_id) {
            if execution.kind == "conflict_resolution" {
                self.finalize_conflict_resolution_attempt(&execution, &outcome)
                    .await;
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

        // Primary path: a PR URL was already captured from a
        // `PostToolUse` Bash hook event (`gh pr create` /
        // `gh pr view` / `gh pr edit` stdout) while the worker was
        // still running. Trust it verbatim, synthesize
        // `PrStatus::Fresh`, and proceed to the in-review transition
        // without shelling out to `jj log` or `gh api commits/.../pulls`.
        //
        // The cold-path fallback below (workspace + detect_pr)
        // remains for engine-restart recovery: if the engine was
        // down when the worker ran `gh pr create`, the in-memory
        // staging cache is empty here and we fall through to
        // reconstruct the URL from local jj state + the GitHub API.
        if let Some(staged_url) = self.staged_pr_urls.get(execution_id) {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "stop event: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            return self
                .finalize_pr_transition(
                    execution_id,
                    staged_url,
                    WorkerPrCompletionTarget::InReview,
                    "stop_staged",
                )
                .await;
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
            .detect_pr(
                &workspace_path,
                &execution.repo_remote_url,
                execution.started_at.as_deref(),
            )
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
                    workspace = %workspace_path.display(),
                    ?err,
                    "stop event: PR detection failed; will retry on next merge-poller sweep"
                );
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
            PrStatus::EmptyDiff { url } => {
                tracing::warn!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    pr_url = %url,
                    "stop event: PR has an empty diff — worker pushed a no-op change; probing to fix or close"
                );
                self.publish_awaiting_pr(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_EMPTY_PR);
                return StopOutcome::EmptyDiffPr { pr_url: url };
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
        // stream, finalize via that URL and skip the detector. This
        // matches the on-Stop shortcut so the merge-poller sweep
        // recovers any chore whose Stop hook fired after the engine
        // restarted (cache empty at Stop) but the PostToolUse for
        // `gh pr create` arrived between then and now.
        if let Some(staged_url) = self.staged_pr_urls.get(execution_id) {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            return self
                .finalize_pr_transition(
                    execution_id,
                    staged_url,
                    WorkerPrCompletionTarget::InReview,
                    "pr_recheck_staged",
                )
                .await;
        }
        let workspace_path = match execution.workspace_path.as_deref() {
            Some(path) => PathBuf::from(path),
            None => return StopOutcome::NoWorkspace,
        };
        let pr_status = match self
            .pr_detector
            .detect_pr(
                &workspace_path,
                &execution.repo_remote_url,
                execution.started_at.as_deref(),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    ?err,
                    "pr-recheck: detector failed; will retry next sweep"
                );
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
            StopOutcome::PrDetected { pr_url }
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
            // for the same worker, or finalize_run racing). Skip.
            StopOutcome::AlreadyTerminal | StopOutcome::UnknownExecution => false,
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
    /// PR exists and head_match, but has zero file changes. The worker
    /// is probed to make real edits or close the PR; the work item
    /// stays in its current state.
    EmptyDiffPr { pr_url: String },
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

    struct StubPrDetector {
        result: Mutex<Result<PrStatus, String>>,
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl StubPrDetector {
        fn ok(value: Option<&str>) -> Arc<Self> {
            let status = match value {
                Some(url) => PrStatus::Fresh { url: url.to_owned() },
                None => PrStatus::None,
            };
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
                call_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn ok_status(status: PrStatus) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
                call_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Err(message.to_owned())),
                call_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn call_count(&self) -> usize {
            self.call_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PrDetector for StubPrDetector {
        async fn detect_pr(
            &self,
            _workspace_path: &Path,
            _repo_remote_url: &str,
            _dispatch_started_at: Option<&str>,
        ) -> Result<PrStatus> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        .with_staged_pr_urls(staged_pr_urls.clone());

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
        .with_staged_pr_urls(staged_pr_urls.clone());

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

    /// Regression for the "worker's `@-` is a recent squash-merge
    /// commit on `main`" misbind (the engine PR-auto-bind regression
    /// where chores were getting stamped with PRs referenced as prior
    /// art in their description text). When the worker did
    /// `jj new main` and committed locally without pushing, `@-`
    /// resolves to the tip of `main` — which is the merge commit of
    /// whatever PR landed most recently. The GitHub
    /// `commits/{sha}/pulls` endpoint then returns that unrelated,
    /// already-merged PR. Without the head-sha gate, the on-Stop
    /// handler would stamp the chore's `pr_url` with that PR and
    /// transition it to `done`.
    ///
    /// The gate: `classify_pr` only returns `Merged`/`Closed` if at
    /// least one local sha matches the PR's `head.sha`. The squash
    /// case keeps `head.sha` = the original PR branch head, which is
    /// a different sha from `@-` on main, so a mismatched merged PR
    /// is correctly rejected as `None`.
    #[test]
    fn classify_pr_rejects_merged_pr_when_head_sha_not_in_local_shas() {
        // Worker's @ is unpushed; @- is the squash-merge commit on
        // main. The merged PR returned by GitHub has `head.sha` =
        // the original branch head, which is neither @ nor @-.
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/99".into(),
            state: "closed".into(),
            merged_at: Some("2026-05-12T03:51:00Z".into()),
            head_sha: "branch_head_sha_aaaaaaaaaaaaaaaaaa".into(),
            changed_files: 3,
            additions: 0,
            deletions: 0,
        };
        let local_shas = vec![
            "worker_at_sha_111111111111111111111".into(),
            "main_tip_sha_222222222222222222222".into(),
        ];
        assert_eq!(classify_pr(pr, &local_shas), PrStatus::None);
    }

    /// Sibling regression: a closed-but-not-merged PR whose `head.sha`
    /// doesn't appear in the worker's local shas is also rejected.
    /// The on-Stop handler treats `None` and `Closed` identically
    /// (publish_awaiting_pr + queue PROBE_NO_PR), so the user-visible
    /// behavior is unchanged for the legitimate "worker pushed, PR got
    /// closed" case — but a phantom closed PR found via `@-` on main
    /// can no longer leak a url onto the chore via downstream code
    /// that reads `PrStatus::url()`.
    #[test]
    fn classify_pr_rejects_closed_pr_when_head_sha_not_in_local_shas() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/100".into(),
            state: "closed".into(),
            merged_at: None,
            head_sha: "branch_head_sha_bbbbbbbbbbbbbbbbbb".into(),
            changed_files: 2,
            additions: 0,
            deletions: 0,
        };
        let local_shas = vec!["worker_at_sha_111111111111111111111".into()];
        assert_eq!(classify_pr(pr, &local_shas), PrStatus::None);
    }

    /// Positive case: a merged PR whose `head.sha` matches a local sha
    /// (the worker pushed and then their PR got merged before Stop
    /// fired) still classifies as `Merged`. This keeps the
    /// `merged_pr_skips_in_review_and_moves_chore_to_done` flow alive
    /// for the legitimate fast-merge case.
    #[test]
    fn classify_pr_accepts_merged_pr_when_head_sha_matches_local() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/42".into(),
            state: "closed".into(),
            merged_at: Some("2026-05-12T04:00:00Z".into()),
            head_sha: "worker_at_sha_111111111111111111111".into(),
            changed_files: 5,
            additions: 0,
            deletions: 0,
        };
        let local_shas = vec![
            "worker_at_sha_111111111111111111111".into(),
            "worker_parent_sha_222222222222222222222".into(),
        ];
        assert_eq!(
            classify_pr(pr, &local_shas),
            PrStatus::Merged {
                url: "https://github.com/foo/bar/pull/42".into(),
            },
        );
    }

    /// Guard: a head-matched PR with all diff-stat fields zero classifies
    /// as `EmptyDiff`, not `Fresh`. The head-sha match confirms the worker
    /// pushed something, but zero files/additions/deletions means the diff
    /// is empty (pending secondary verification in `detect_pr`).
    #[test]
    fn classify_pr_returns_empty_diff_when_all_diff_stats_are_zero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/55".into(),
            state: "open".into(),
            merged_at: None,
            head_sha: "worker_at_sha_111111111111111111111".into(),
            changed_files: 0,
            additions: 0,
            deletions: 0,
        };
        let local_shas = vec![
            "worker_at_sha_111111111111111111111".into(),
            "worker_parent_sha_222222222222222222222".into(),
        ];
        assert_eq!(
            classify_pr(pr, &local_shas),
            PrStatus::EmptyDiff {
                url: "https://github.com/foo/bar/pull/55".into(),
            },
        );
    }

    /// Regression: `changed_files == 0` must NOT produce `EmptyDiff` when
    /// `additions` or `deletions` are non-zero.  This is the false-positive
    /// scenario observed with PR #446: GitHub's `commits/{sha}/pulls`
    /// endpoint returned `changed_files: 0` (async diff-stat lag) while
    /// `additions: 1, deletions: 1` were already computed.  Before this
    /// fix the engine injected a bogus "your diff is empty" directive into
    /// the worker pane on every Stop event.
    #[test]
    fn classify_pr_returns_fresh_when_changed_files_zero_but_additions_nonzero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/446".into(),
            state: "open".into(),
            merged_at: None,
            head_sha: "worker_at_sha_111111111111111111111".into(),
            changed_files: 0,
            additions: 1,
            deletions: 1,
        };
        let local_shas = vec!["worker_at_sha_111111111111111111111".into()];
        assert_eq!(
            classify_pr(pr, &local_shas),
            PrStatus::Fresh {
                url: "https://github.com/foo/bar/pull/446".into(),
            },
        );
    }

    /// Confirm that a head-matched PR with `changed_files > 0` still
    /// classifies as `Fresh`.
    #[test]
    fn classify_pr_returns_fresh_when_head_matches_and_changed_files_nonzero() {
        let pr = ApiPr {
            url: "https://github.com/foo/bar/pull/56".into(),
            state: "open".into(),
            merged_at: None,
            head_sha: "worker_at_sha_111111111111111111111".into(),
            changed_files: 1,
            additions: 0,
            deletions: 0,
        };
        let local_shas = vec!["worker_at_sha_111111111111111111111".into()];
        assert_eq!(
            classify_pr(pr, &local_shas),
            PrStatus::Fresh {
                url: "https://github.com/foo/bar/pull/56".into(),
            },
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

    /// The bookmark expansion in `jj_candidate_commit_shas` is gated
    /// on `dispatch_started_at` being present and well-formed. Pin the
    /// revset string shape directly so the production query stays
    /// surgical: legacy callers without a timestamp keep the
    /// pre-fix `@ | @-`-only behaviour; callers with a timestamp get
    /// the bookmark tip expansion; pathological inputs (embedded
    /// double quotes) fail closed by dropping the expansion rather
    /// than producing an invalid revset that would fail the whole
    /// detection pass.
    #[test]
    fn build_candidate_revset_with_no_timestamp_keeps_legacy_revset() {
        assert_eq!(build_candidate_revset(None), "@ | @-");
        assert_eq!(build_candidate_revset(Some("")), "@ | @-");
        assert_eq!(build_candidate_revset(Some("   ")), "@ | @-");
    }

    #[test]
    fn build_candidate_revset_with_timestamp_expands_to_recent_bookmarks() {
        // Engine writes started_at as unix epoch seconds (now_string()).
        // 0 == 1970-01-01T00:00:00Z (Unix epoch origin).
        assert_eq!(
            build_candidate_revset(Some("0")),
            r#"@ | @- | (bookmarks() & committer_date(after:"1970-01-01T00:00:00Z"))"#,
        );
        // 946684800 == 2000-01-01T00:00:00Z.
        assert_eq!(
            build_candidate_revset(Some("946684800")),
            r#"@ | @- | (bookmarks() & committer_date(after:"2000-01-01T00:00:00Z"))"#,
        );
    }

    #[test]
    fn build_candidate_revset_drops_expansion_when_timestamp_is_non_numeric() {
        // Non-numeric input (e.g. an old RFC 3339 string or junk) cannot be
        // converted; fail closed to the legacy revset rather than producing
        // an invalid jj query that would fail the whole detection pass.
        assert_eq!(
            build_candidate_revset(Some("2026-05-13T21:00:00Z")),
            "@ | @-",
        );
        assert_eq!(build_candidate_revset(Some("not-a-number")), "@ | @-");
    }

    /// Real-jj regression for the bookmark-tip candidate expansion.
    /// Initialises a colocated jj workspace, commits on a named
    /// bookmark, then moves `@` to the root commit (mirroring a
    /// worker that pushed and then did `jj new main`). With the
    /// dispatch timestamp set, `jj_candidate_commit_shas` must return
    /// the bookmark tip so the downstream detector can find the PR
    /// the worker actually opened. Without the timestamp, the legacy
    /// `@ | @-`-only revset must still apply — preserving behaviour
    /// for callers that have not opted into the expansion.
    #[tokio::test]
    async fn jj_candidate_commit_shas_includes_recent_bookmark_tip() {
        // Skip when `jj` is unavailable on $PATH (e.g. minimal CI
        // images). The cube-workspace assumption — jj is installed on
        // the host — holds in our dispatch environment.
        let jj_available = std::process::Command::new("jj")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !jj_available {
            eprintln!("jj not available on PATH — skipping");
            return;
        }

        let workspace = tempdir().unwrap();
        let ws_path = workspace.path();
        let run_jj = |args: &[&str]| {
            std::process::Command::new("jj")
                .args(args)
                .current_dir(ws_path)
                .env("JJ_USER", "test")
                .env("JJ_EMAIL", "test@example.com")
                .output()
                .expect("jj command failed to spawn")
        };

        // Bootstrap: jj-only repo (no .git colocate needed).
        let init = run_jj(&["git", "init"]);
        assert!(
            init.status.success(),
            "jj git init failed: {}",
            String::from_utf8_lossy(&init.stderr),
        );

        // Commit the worker's "fix" on a named bookmark, then push `@`
        // past it. Mirrors: `jj describe -m worker-fix && jj bookmark
        // create my-fix -r @ && jj git push -b my-fix && jj new root()`.
        // We can't run an actual push here (no remote), but the
        // candidate-shas function reads local refs, not remote-tracking
        // state — the bookmark existing locally is sufficient.
        let describe = run_jj(&["describe", "-m", "worker-fix-commit"]);
        assert!(
            describe.status.success(),
            "jj describe failed: {}",
            String::from_utf8_lossy(&describe.stderr),
        );
        let create_bookmark = run_jj(&["bookmark", "create", "my-fix", "-r", "@"]);
        assert!(
            create_bookmark.status.success(),
            "jj bookmark create failed: {}",
            String::from_utf8_lossy(&create_bookmark.stderr),
        );
        // Capture the bookmark tip sha.
        let bookmark_tip = run_jj(&[
            "log",
            "--no-graph",
            "-r",
            "my-fix",
            "-T",
            r#"commit_id ++ "\n""#,
        ]);
        assert!(bookmark_tip.status.success());
        let bookmark_tip_sha = String::from_utf8_lossy(&bookmark_tip.stdout)
            .trim()
            .to_owned();
        assert_eq!(
            bookmark_tip_sha.len(),
            40,
            "expected a 40-char commit id, got {:?}",
            bookmark_tip_sha,
        );

        // Now move `@` to a fresh commit off `root()` — mirrors the
        // worker doing `jj new main` after `jj git push`. The
        // bookmark stays where it is, but `@` and `@-` no longer
        // reach the bookmark tip.
        let new_off_root = run_jj(&["new", "root()"]);
        assert!(
            new_off_root.status.success(),
            "jj new root() failed: {}",
            String::from_utf8_lossy(&new_off_root.stderr),
        );

        // With no timestamp — legacy revset — the bookmark tip must
        // NOT appear. This is the pre-fix bug: `@ | @-` doesn't
        // reach the worker's pushed commit.
        let legacy = jj_candidate_commit_shas(ws_path, None)
            .await
            .expect("legacy candidate query failed");
        assert!(
            !legacy.contains(&bookmark_tip_sha),
            "legacy revset must NOT include the bookmark tip (the bug): got {legacy:?}",
        );

        // With a past timestamp (unix epoch seconds, matching the engine's
        // now_string() format) — fix engaged — the bookmark tip MUST appear
        // in the candidate set. Using 946684800 == 2000-01-01T00:00:00Z.
        // This is the format the engine actually writes; the prior test used
        // ISO 8601 which masked the production bug.
        let with_since = jj_candidate_commit_shas(ws_path, Some("946684800"))
            .await
            .expect("with-since candidate query failed");
        assert!(
            with_since.contains(&bookmark_tip_sha),
            "started_at-gated revset must include the bookmark tip: got {with_since:?}",
        );

        // With a future timestamp (unix epoch seconds far in the future) —
        // bookmark tip is older than the dispatch window — the bookmark tip
        // must NOT appear. Using 32503680000 == 3000-01-01T00:00:00Z.
        let with_future = jj_candidate_commit_shas(ws_path, Some("32503680000"))
            .await
            .expect("future-since candidate query failed");
        assert!(
            !with_future.contains(&bookmark_tip_sha),
            "future-dated started_at must exclude older bookmark tips: got {with_future:?}",
        );
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
