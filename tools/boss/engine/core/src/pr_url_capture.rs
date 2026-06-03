//! Primary-path PR URL capture from worker hook events.
//!
//! Background: the engine receives the URL of every PR a worker
//! opens in real time, embedded in the `tool_response.stdout` of the
//! `PostToolUse` hook event for the worker's `gh pr create` Bash
//! call. Historically the engine ignored that and reconstructed the
//! URL later by shelling out to `jj log` against the worker's cube
//! workspace and querying the GitHub API for each candidate commit
//! sha. That reconstruction path is fragile (it failed once when
//! the worker did `jj new main` after pushing; it failed again when
//! a date-format mismatch broke the bookmark-tip revset expansion)
//! and unnecessary — the URL is literally already in the event
//! stream.
//!
//! This module exposes the two pieces the primary path needs:
//!
//! - [`extract_pr_url_from_bash_response`] — a pure regex scan over
//!   a `tool_response` JSON value. Returns the first canonical
//!   `https://github.com/<owner>/<repo>/pull/<N>` it finds in either
//!   `stdout` or `stderr`. Pure, easy to test.
//! - [`StagedPrUrlCache`] — a thread-safe `HashMap<execution_id,
//!   pr_url>` that callers populate from PostToolUse events and the
//!   `on_stop` handler reads on Stop. First-writer-wins semantics
//!   so a worker that re-runs `gh pr view` after `gh pr create`
//!   can't overwrite the legitimate first URL.
//!
//! The reconciliation path (`completion::detect_pr` →
//! `jj_candidate_commit_shas` → GitHub commits/{sha}/pulls) is
//! preserved as the engine-restart recovery fallback. If the engine
//! restarts after a worker pushed but before Stop fired, the staged
//! URL is lost from this cache (it lives in memory only) and the
//! fallback path runs on the next sweep. The staging cache is the
//! hot path; the reconstruction path is the cold path.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

/// Canonical PR URL pattern: `https://github.com/<owner>/<repo>/pull/<N>`.
/// Owner / repo accept `[A-Za-z0-9._-]+` (GitHub's actual character
/// set). The PR number is captured but the function returns the full
/// matched URL so callers can use it verbatim.
///
/// Trailing path components (`/files`, `/commits`, `#issuecomment-…`,
/// query strings) are *not* matched into the canonical form — the
/// regex stops at the digit run, so a URL like
/// `https://github.com/owner/repo/pull/123/files` returns
/// `https://github.com/owner/repo/pull/123`.
static PR_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://github\.com/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/pull/\d+")
        .expect("PR URL regex compiles")
});

/// Captures the `owner/repo` slug from a PR URL.
static PR_URL_SLUG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://github\.com/([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)/pull/\d+")
        .expect("PR URL slug regex compiles")
});

/// Well-known placeholder owner/repo slugs used in tests and documentation
/// (compared case-insensitively). These are rejected as a belt-and-suspenders
/// check even before the product-repo gate runs.
static PLACEHOLDER_SLUGS: &[&str] = &[
    "foo/bar",
    "octocat/hello-world",
    "someuser/somerepo",
    "example/example",
];

/// Parse `product_repo_remote_url` (SSH `git@github.com:owner/repo.git` or
/// HTTPS `https://github.com/owner/repo`) into a lowercase `owner/repo` slug.
/// Returns `None` if the URL is not a recognisable github.com remote.
pub fn parse_product_slug(repo_remote_url: &str) -> Option<String> {
    let (owner, repo) = boss_github::repo_slug::parse_github_owner_repo(repo_remote_url).ok()?;
    Some(format!("{}/{}", owner.to_lowercase(), repo.to_lowercase()))
}

/// Validate that `pr_url` belongs to the product identified by
/// `product_repo_remote_url`. Returns `Ok(())` when the URL is a
/// legitimate product PR, or `Err(reason)` explaining why it was
/// rejected.
///
/// Two gates run in order:
/// 1. **Placeholder reject** — slugs from `PLACEHOLDER_SLUGS` are
///    dropped immediately with an informative reason. These are test
///    fixtures that should never appear in real worker output.
/// 2. **Repo-remote-url gate** — the URL's `owner/repo` must
///    case-insensitively match the parsed slug of
///    `product_repo_remote_url`. A worker operating on the product's
///    cube workspace can only legitimately emit a PR URL for that repo.
pub fn validate_pr_url(pr_url: &str, product_repo_remote_url: &str) -> Result<(), String> {
    let pr_slug = PR_URL_SLUG_RE
        .captures(pr_url)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_lowercase())
        .ok_or_else(|| format!("URL does not contain a recognisable owner/repo slug: {pr_url}"))?;

    if PLACEHOLDER_SLUGS.iter().any(|p| pr_slug == *p) {
        return Err(format!(
            "owner/repo `{pr_slug}` is a well-known test placeholder"
        ));
    }

    let product_slug = parse_product_slug(product_repo_remote_url).ok_or_else(|| {
        format!("could not parse product repo slug from `{product_repo_remote_url}`")
    })?;

    if pr_slug != product_slug {
        return Err(format!(
            "URL repo `{pr_slug}` does not match product repo `{product_slug}`"
        ));
    }

    Ok(())
}

/// Scan a `tool_response` JSON value for a GitHub PR URL.
///
/// Reads the `stdout` and `stderr` fields (both are strings in the
/// claude-code Bash tool response shape) and returns the first
/// canonical pull URL it finds, or `None` if neither field carries
/// one. `stdout` is checked first because `gh pr create` and
/// `gh pr view` both print the URL there; `stderr` is the fallback
/// for shell configurations / wrapper scripts that redirect.
///
/// The regex is anchored to `https://github.com/` — heuristic
/// strings the worker might emit ("see the PR at …", "PR #458 is
/// ready") that don't carry the full URL are ignored. We want a
/// captured URL we can write verbatim to `tasks.pr_url`, not a
/// pattern that might bind us to the wrong repo.
pub fn extract_pr_url_from_bash_response(tool_response: &serde_json::Value) -> Option<String> {
    let scan = |field: &str| -> Option<String> {
        let text = tool_response.get(field)?.as_str()?;
        PR_URL_RE
            .find(text)
            .map(|m| m.as_str().to_owned())
    };
    scan("stdout").or_else(|| scan("stderr"))
}

/// Check whether a Bash `tool_input` command is a deliberate `gh pr`
/// invocation (create, view, list, or edit).
///
/// Returns `true` only when the Bash command string is a
/// `gh pr <subcommand>` invocation, where the subcommand is one of the
/// forms that can legitimately surface a PR URL for the worker's own
/// PR. Handles environment-variable prefixes such as
/// `GIT_DIR=.jj/repo/store/git gh pr create ...` via the shared
/// [`crate::gh_invocation::classify`] matcher.
///
/// Use this as the Layer-1 gate in the PostToolUse capture path:
/// arbitrary Bash commands whose output happens to contain a PR URL
/// (file reads, test runs, chore descriptions echoed via shell) must
/// not stage a wrong PR against the running execution.
pub fn is_gh_pr_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    // `cube pr ensure` is the jj-aware create-or-reuse wrapper that
    // outputs a PR URL as its only stdout line — treat it the same as
    // `gh pr create` for capture purposes. It is not a `gh` invocation,
    // so the shared classifier doesn't see it; check it directly.
    if command.contains("cube pr ensure") {
        return true;
    }
    matches!(
        crate::gh_invocation::classify(command),
        Some(inv)
            if inv.noun == crate::gh_invocation::GhNoun::Pr
                && matches!(inv.subcommand.as_str(), "create" | "view" | "list" | "edit")
    )
}

/// Outcome of [`StagedPrUrlCache::record_if_unset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagePrUrlOutcome {
    /// The URL was new for this execution and is now staged.
    Staged,
    /// An earlier event already staged a URL for this execution; the
    /// new value was ignored (first-writer-wins).
    AlreadyStaged,
}

/// In-memory `execution_id → pr_url` staging cache. Populated by the
/// `PostToolUse` hook dispatcher when a Bash event surfaces a PR
/// URL; consumed by `WorkerCompletionHandler::on_stop` on the
/// matching Stop hook.
///
/// First-writer-wins. A worker that pushes, opens a PR (URL latched),
/// then later runs `gh pr view <other-PR>` while editing — the later
/// `view` doesn't clobber the legitimate first `create`.
#[derive(Debug, Default)]
pub struct StagedPrUrlCache {
    inner: Mutex<HashMap<String, String>>,
}

impl StagedPrUrlCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage `pr_url` against `execution_id` if no URL is currently
    /// staged. Returns whether the staging happened or was skipped.
    pub fn record_if_unset(&self, execution_id: &str, pr_url: &str) -> StagePrUrlOutcome {
        let mut guard = self
            .inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned");
        if guard.contains_key(execution_id) {
            StagePrUrlOutcome::AlreadyStaged
        } else {
            guard.insert(execution_id.to_owned(), pr_url.to_owned());
            StagePrUrlOutcome::Staged
        }
    }

    /// Read the staged URL for `execution_id`, if any. Does not
    /// remove the entry — callers that want to clear should call
    /// [`Self::forget`].
    pub fn get(&self, execution_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned")
            .get(execution_id)
            .cloned()
    }

    /// Drop any staged URL for `execution_id`. Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_from_gh_pr_create_stdout() {
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_returns_canonical_form_stripping_trailing_path() {
        // `gh pr view --json url` sometimes emits the URL inside a
        // JSON blob; the URL itself is canonical. Other surfaces
        // (issue comments, PR pages) may include `/files`,
        // `/commits`, `#issuecomment-…`. The regex stops at the
        // PR number so we never bind to a sub-path.
        let response = json!({
            "stdout": "{\"url\":\"https://github.com/spinyfin/mono/pull/458/files#diff-abc\"}",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_falls_back_to_stderr_when_stdout_absent() {
        let response = json!({
            "stdout": "",
            "stderr": "Created pull request: https://github.com/spinyfin/mono/pull/458\n",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_prefers_stdout_over_stderr() {
        // If both surfaces carry a URL — e.g. the worker piped
        // `gh pr create` output through a wrapper that also logged
        // a previously-cached URL to stderr — stdout wins because
        // it's the canonical output of the just-run command.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "https://github.com/spinyfin/mono/pull/100",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_returns_first_match_when_stdout_has_multiple() {
        // A worker that runs `gh pr view 100 && gh pr create` in a
        // single Bash call could surface two URLs. The first is the
        // one we want — chronologically, it's the one printed by
        // the earlier command, but more importantly any later URL
        // in stdout is most often the just-created one's URL
        // followed by a CI status line containing a different
        // checks URL. We don't try to disambiguate; we take the
        // first match deterministically and document that workers
        // should keep `gh pr create` in its own Bash call.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/100\nhttps://github.com/spinyfin/mono/pull/458\n",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/100"),
        );
    }

    #[test]
    fn extract_returns_none_when_no_url() {
        let response = json!({
            "stdout": "Hello world\n",
            "stderr": "",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_returns_none_when_response_is_not_an_object() {
        let response = json!("just a string");
        assert_eq!(extract_pr_url_from_bash_response(&response), None);

        let response = json!(null);
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_ignores_non_github_pull_urls() {
        // A worker mentioning a pull URL on a different host (e.g.
        // gitlab, gitea) is not a GitHub PR. The engine's binding
        // path is keyed on GitHub repo slugs; non-github URLs must
        // not latch.
        let response = json!({
            "stdout": "https://gitlab.com/x/y/-/merge_requests/123\n",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_ignores_issue_urls() {
        // GitHub issue URLs use `/issues/<N>`, not `/pull/<N>`.
        // The regex is anchored on `/pull/` so they don't match.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/issues/300\n",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_pulls_url_from_real_gh_pr_create_output() {
        // Reproduces a real PostToolUse `tool_response` shape as
        // observed in `/tmp/boss-engine.log` for Riker's
        // `exec_18af43101ae56430_6` (2026-05-13 23:20:31Z). The
        // body field exists alongside the URL — `gh pr create`
        // only ever prints the URL to stdout, but the surrounding
        // JSON is richer.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": false,
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    // ── StagedPrUrlCache ──────────────────────────────────────────

    #[test]
    fn cache_records_first_url_for_an_execution() {
        let cache = StagedPrUrlCache::new();
        let outcome = cache.record_if_unset(
            "exec_abc",
            "https://github.com/spinyfin/mono/pull/458",
        );
        assert_eq!(outcome, StagePrUrlOutcome::Staged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn cache_ignores_subsequent_records_for_same_execution() {
        // The worker that pushed and ran `gh pr create` (URL latched)
        // and later ran `gh pr view <some-other-PR>` in a follow-up
        // Bash call (e.g. inspecting a referenced PR) must not have
        // the staged URL clobbered. First-writer-wins.
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset(
            "exec_abc",
            "https://github.com/spinyfin/mono/pull/458",
        );
        let outcome = cache.record_if_unset(
            "exec_abc",
            "https://github.com/spinyfin/mono/pull/999",
        );
        assert_eq!(outcome, StagePrUrlOutcome::AlreadyStaged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn cache_isolates_executions() {
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset(
            "exec_a",
            "https://github.com/spinyfin/mono/pull/1",
        );
        cache.record_if_unset(
            "exec_b",
            "https://github.com/spinyfin/mono/pull/2",
        );
        assert_eq!(
            cache.get("exec_a").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/1"),
        );
        assert_eq!(
            cache.get("exec_b").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/2"),
        );
    }

    #[test]
    fn cache_forget_drops_entry_and_allows_re_record() {
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset(
            "exec_abc",
            "https://github.com/spinyfin/mono/pull/458",
        );
        cache.forget("exec_abc");
        assert_eq!(cache.get("exec_abc"), None);
        // A fresh record after forget should succeed — useful if
        // the same execution_id gets reused (it shouldn't in prod,
        // but the semantics are: forget clears state).
        let outcome = cache.record_if_unset(
            "exec_abc",
            "https://github.com/spinyfin/mono/pull/999",
        );
        assert_eq!(outcome, StagePrUrlOutcome::Staged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/999"),
        );
    }

    #[test]
    fn cache_forget_is_idempotent() {
        let cache = StagedPrUrlCache::new();
        cache.forget("never-staged");
        cache.forget("never-staged");
        assert_eq!(cache.get("never-staged"), None);
    }

    // ── is_gh_pr_command ──────────────────────────────────────────

    #[test]
    fn gh_pr_create_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({
            "command": "gh pr create --head boss/exec_abc --base main --title 'fix: something'"
        })));
    }

    #[test]
    fn gh_pr_create_with_git_dir_prefix_is_a_gh_pr_command() {
        // Workers use GIT_DIR=.jj/repo/store/git because jj-backed
        // workspaces lack a top-level .git directory.
        assert!(is_gh_pr_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr create --head boss/exec_abc --base main"
        })));
    }

    #[test]
    fn gh_pr_view_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr view"
        })));
    }

    #[test]
    fn gh_pr_list_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({ "command": "gh pr list --state open" })));
    }

    #[test]
    fn gh_pr_edit_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({ "command": "gh pr edit 42 --add-label foo" })));
    }

    #[test]
    fn cube_pr_ensure_is_a_gh_pr_command() {
        // `cube pr ensure` outputs a PR URL as its only stdout line and
        // must be captured the same way as `gh pr create`.
        assert!(is_gh_pr_command(&json!({
            "command": "cube pr ensure --branch boss/exec_abc123_01 --title 'my feature'"
        })));
    }

    #[test]
    fn non_gh_command_is_not_a_gh_pr_command() {
        // Bash command that outputs PR URLs (e.g. reading a chore
        // description that mentions a prior PR) must not trigger capture.
        assert!(!is_gh_pr_command(&json!({
            "command": "bossctl task show task_123"
        })));
    }

    #[test]
    fn cat_command_with_pr_url_content_is_not_a_gh_pr_command() {
        assert!(!is_gh_pr_command(&json!({ "command": "cat chore.md" })));
    }

    #[test]
    fn grep_command_is_not_a_gh_pr_command() {
        assert!(!is_gh_pr_command(&json!({
            "command": "grep -r 'pull/' . | head -5"
        })));
    }

    #[test]
    fn gh_issue_is_not_a_gh_pr_command() {
        // `gh issue` is not a PR command.
        assert!(!is_gh_pr_command(&json!({ "command": "gh issue list" })));
    }

    #[test]
    fn missing_command_field_returns_false() {
        assert!(!is_gh_pr_command(&json!({ "timeout": 30000 })));
    }

    #[test]
    fn null_tool_input_returns_false() {
        assert!(!is_gh_pr_command(&json!(null)));
    }

    // ── validate_pr_url ───────────────────────────────────────────

    #[test]
    fn validate_rejects_foo_bar_placeholder() {
        // Simulates a worker that emits a foo/bar fixture URL in test
        // output captured by a PostToolUse event.
        let response = json!({
            "stdout": "Pull request created: https://github.com/foo/bar/pull/42",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        let result = validate_pr_url(
            &extracted,
            "git@github.com:spinyfin/mono.git",
        );
        assert!(result.is_err(), "foo/bar should be rejected");
        let reason = result.unwrap_err();
        assert!(
            reason.contains("placeholder"),
            "rejection reason should mention placeholder, got: {reason}",
        );
    }

    #[test]
    fn validate_accepts_product_repo_url() {
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/42",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        assert_eq!(
            validate_pr_url(&extracted, "git@github.com:spinyfin/mono.git"),
            Ok(()),
        );
    }

    #[test]
    fn validate_rejects_octocat_hello_world_placeholder() {
        let response = json!({
            "stdout": "https://github.com/octocat/Hello-World/pull/1",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        let result = validate_pr_url(
            &extracted,
            "git@github.com:spinyfin/mono.git",
        );
        assert!(result.is_err(), "octocat/Hello-World should be rejected");
    }

    #[test]
    fn validate_rejects_url_for_wrong_repo() {
        // A worker running tests that mention another GitHub repo's PR URL.
        let result = validate_pr_url(
            "https://github.com/some-org/other-repo/pull/10",
            "git@github.com:spinyfin/mono.git",
        );
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(reason.contains("does not match"), "got: {reason}");
    }

    #[test]
    fn parse_product_slug_handles_ssh_and_https() {
        assert_eq!(
            parse_product_slug("git@github.com:spinyfin/mono.git"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(
            parse_product_slug("https://github.com/spinyfin/mono.git"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(
            parse_product_slug("https://github.com/spinyfin/mono"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(parse_product_slug("https://gitlab.com/foo/bar"), None);
    }

    #[test]
    fn validate_is_case_insensitive_for_slug_matching() {
        // GitHub names are case-insensitive; SpinYFin/Mono must match
        // spinyfin/mono from the product's repo_remote_url.
        let result = validate_pr_url(
            "https://github.com/SpinYFin/Mono/pull/99",
            "git@github.com:spinyfin/mono.git",
        );
        assert_eq!(result, Ok(()));
    }
}
