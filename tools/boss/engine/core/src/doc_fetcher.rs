//! Live design-doc fetcher.
//!
//! Wraps [`boss_github::contents::fetch_repo_file`] with Boss-specific
//! retry logic and typed outcomes. Both this module and
//! [`crate::attentions_detector`] share that single fetch implementation.
//!
//! This is an independent root component that feeds the Populator
//! (task 7 of the auto-populate-project-tasks-on-design-pr-merge design).
//! It has no knowledge of the Planner or Materializer it feeds.
//!
//! ## Error handling
//!
//! - **404 (file missing at the ref):** returned as [`DocFetchOutcome::DocMissing`]
//!   immediately, with no retries, because retrying a 404 cannot help.
//! - **Transient errors (5xx, transport failures):** retried up to
//!   [`MAX_FETCH_ATTEMPTS`] times with a short fixed delay. On exhaustion,
//!   [`DocFetchOutcome::FetchFailed`] is returned.
//! - **Unparseable `repo_remote_url`:** returned as
//!   [`DocFetchOutcome::FetchFailed`] immediately, before any `gh` call.

use std::time::Duration;

use tokio::time::sleep;

/// Maximum number of `gh api` attempts. Covers the initial attempt plus two
/// retries — enough to survive a brief GitHub 5xx or transient network blip
/// without holding the Populator slot for a long time.
const MAX_FETCH_ATTEMPTS: u32 = 3;

/// Fixed wait between retries. Short enough that three attempts fit inside any
/// reasonable engine loop tick budget, long enough that a transient 503 can
/// clear.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Typed outcome of a [`fetch_design_doc`] call.
#[derive(Debug)]
pub enum DocFetchOutcome {
    /// The document was fetched successfully. Contains the raw UTF-8 content.
    Content(String),
    /// The path does not exist at the given ref (HTTP 404). No retry was
    /// attempted; none would help. Maps to `outcome = 'doc_missing'` in the
    /// `planner_runs` audit ledger.
    DocMissing,
    /// All fetch attempts were exhausted due to transient or configuration
    /// errors. `reason` is the last error message. Maps to
    /// `outcome = 'fetch_failed'` in the audit ledger.
    FetchFailed { reason: String },
}

/// Fetch the raw content of `doc_path` from `repo_remote_url` at `ref_name`.
///
/// `repo_remote_url` is any GitHub remote URL shape accepted by
/// [`git_utils::repo_slug::parse_github_owner_repo`]:
/// `https://github.com/owner/repo`, `git@github.com:owner/repo.git`, etc.
///
/// `ref_name` is the merged branch name or commit sha the Populator receives
/// from the merge poller (e.g. `"main"` or a commit sha). Slashed branch names
/// like `boss/exec_*` are handled correctly by passing `ref` as a `-f` query
/// field with `--method GET` so `gh` URL-encodes the `/` for us.
pub async fn fetch_design_doc(
    repo_remote_url: &str,
    doc_path: &str,
    ref_name: &str,
) -> DocFetchOutcome {
    let (owner, repo) = match git_utils::repo_slug::parse_github_owner_repo(repo_remote_url) {
        Ok(pair) => pair,
        Err(err) => {
            return DocFetchOutcome::FetchFailed {
                reason: format!(
                    "cannot derive owner/repo from repo_remote_url {repo_remote_url:?}: {err}"
                ),
            };
        }
    };

    let mut last_reason = String::new();
    for attempt in 1..=MAX_FETCH_ATTEMPTS {
        match do_fetch(owner, repo, doc_path, ref_name).await {
            FetchResult::Content(text) => return DocFetchOutcome::Content(text),
            FetchResult::NotFound => return DocFetchOutcome::DocMissing,
            FetchResult::Error(reason) => {
                tracing::warn!(
                    repo_remote_url,
                    doc_path,
                    ref_name,
                    attempt,
                    reason,
                    "doc fetcher: gh api attempt failed"
                );
                last_reason = reason;
                if attempt < MAX_FETCH_ATTEMPTS {
                    sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    DocFetchOutcome::FetchFailed { reason: last_reason }
}

/// Internal result of a single `gh api` call, before retry logic is applied.
enum FetchResult {
    Content(String),
    NotFound,
    Error(String),
}

async fn do_fetch(owner: &str, repo: &str, path: &str, ref_name: &str) -> FetchResult {
    match boss_github::contents::fetch_repo_file(owner, repo, path, ref_name).await {
        Ok(Some(content)) => FetchResult::Content(content),
        Ok(None) => FetchResult::NotFound,
        Err(err) => FetchResult::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_failed_on_unparseable_url() {
        let outcome = fetch_design_doc("not-a-valid-url", "some/path.md", "main").await;
        assert!(
            matches!(outcome, DocFetchOutcome::FetchFailed { .. }),
            "expected FetchFailed for an unparseable repo_remote_url"
        );
    }

    #[tokio::test]
    async fn fetch_failed_on_non_github_url() {
        let outcome = fetch_design_doc("https://gitlab.com/owner/repo", "path.md", "main").await;
        assert!(
            matches!(outcome, DocFetchOutcome::FetchFailed { .. }),
            "expected FetchFailed for a non-github URL"
        );
    }
}
