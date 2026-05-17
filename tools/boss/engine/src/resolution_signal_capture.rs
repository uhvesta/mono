//! Primary-path resolution-signal capture from worker hook events.
//!
//! A conflict-resolution worker that successfully resolves merge conflicts
//! typically emits two observable signals before stopping:
//!
//! 1. A force-push of the rebased branch (`jj git push --force` or
//!    `git push -f`). Observable in `PostToolUse` `tool_input.command`.
//! 2. A PR comment announcing the resolution
//!    (`gh pr comment <N> --body "…"`). Observable in `PostToolUse`
//!    `tool_response.stdout` as an `#issuecomment-…` URL.
//!
//! This module exposes:
//!
//! - [`is_force_push_command`] — detects force-push Bash commands.
//! - [`extract_resolution_comment_url`] — scans a `tool_response` JSON
//!   value for a `#issuecomment-…` URL.
//! - [`StagedResolutionSignalCache`] — an `execution_id → signal set`
//!   map populated by the `PostToolUse` dispatcher in `app.rs` and
//!   consumed by `WorkerCompletionHandler::on_stop` for the primary-path
//!   `blocked → in_review` parent-chore transition.
//!
//! Any one signal is sufficient for the transition; the cache accumulates
//! all signals observed for an execution (idempotent per kind).
//!
//! The merge-poller sweep remains as the cold-path fallback for the
//! engine-restart case (cache empty because the engine restarted after
//! the worker pushed but before Stop fired) and for force-push command
//! variants that the patterns here don't match.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use regex::Regex;

/// Observable signal kinds emitted by a successful conflict-resolution worker.
///
/// Each variant is independently detectable from `PostToolUse` events.
/// Any one is sufficient to drive the `blocked → in_review` primary-path
/// transition on Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolutionSignal {
    /// Worker ran a force-push (`jj git push --force` / `git push -f`),
    /// indicating it rebased / merged and pushed the resolved branch.
    ForcePushed,
    /// Worker posted a PR comment whose `tool_response.stdout` contains an
    /// `#issuecomment-<N>` URL — the canonical "resolution announced" signal
    /// emitted by `gh pr comment`.
    ResolutionCommentPosted,
}

/// GitHub PR issue-comment URL — the stdout shape of a successful
/// `gh pr comment` call. Path ends in `#issuecomment-<digits>`.
static ISSUECOMMENT_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https://github\.com/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/pull/\d+#issuecomment-\d+",
    )
    .expect("issuecomment URL regex compiles")
});

/// Detect a force-push command in a Bash `tool_input`.
///
/// Returns `true` when the command string contains `git push` (including
/// `jj git push`) with a force flag: `--force`, `--force-with-lease`, or
/// a standalone `-f` token. Matching by token avoids false positives on
/// filenames containing `-f`.
pub fn is_force_push_command(tool_input: &serde_json::Value) -> bool {
    let command = match tool_input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return false,
    };
    if !command.contains("git push") {
        return false;
    }
    command.split_whitespace().any(|token| {
        token == "-f" || token == "--force" || token == "--force-with-lease"
    })
}

/// Scan `tool_response` for a GitHub PR issue-comment URL — the stdout
/// shape of a successful `gh pr comment` call.
///
/// Returns the first `#issuecomment-<N>` URL found in `stdout` (then
/// `stderr` as fallback), or `None`.
pub fn extract_resolution_comment_url(
    tool_response: &serde_json::Value,
) -> Option<String> {
    let scan = |field: &str| -> Option<String> {
        let text = tool_response.get(field)?.as_str()?;
        ISSUECOMMENT_URL_RE
            .find(text)
            .map(|m| m.as_str().to_owned())
    };
    scan("stdout").or_else(|| scan("stderr"))
}

/// In-memory `execution_id → set<ResolutionSignal>` staging cache.
///
/// Populated by the `PostToolUse` dispatcher in `app.rs` when Bash
/// events for a `conflict_resolution` execution match the force-push or
/// PR-comment patterns. Consumed by
/// `WorkerCompletionHandler::on_stop` on the matching Stop hook.
///
/// Signal sets grow monotonically — once a signal is recorded it stays
/// until [`Self::forget`] is called. Multiple signals for one execution
/// accumulate; callers need only [`Self::has_any_signal`].
#[derive(Debug, Default)]
pub struct StagedResolutionSignalCache {
    inner: Mutex<HashMap<String, HashSet<ResolutionSignal>>>,
}

impl StagedResolutionSignalCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `signal` against `execution_id`. Idempotent per signal
    /// kind — recording the same signal twice is a no-op. Multiple
    /// distinct signals for one execution accumulate in the set.
    pub fn record_signal(&self, execution_id: &str, signal: ResolutionSignal) {
        let mut guard = self
            .inner
            .lock()
            .expect("StagedResolutionSignalCache mutex poisoned");
        guard
            .entry(execution_id.to_owned())
            .or_default()
            .insert(signal);
    }

    /// Return `true` if at least one signal has been recorded for
    /// `execution_id`.
    pub fn has_any_signal(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("StagedResolutionSignalCache mutex poisoned")
            .get(execution_id)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    /// Drop all signals for `execution_id`. Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("StagedResolutionSignalCache mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── is_force_push_command ─────────────────────────────────────

    #[test]
    fn jj_git_push_force_flag_is_force_push() {
        assert!(is_force_push_command(&json!({
            "command": "jj git push -b my-branch --force"
        })));
    }

    #[test]
    fn jj_git_push_long_force_flag_is_force_push() {
        assert!(is_force_push_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git jj git push -b boss/exec_abc --force"
        })));
    }

    #[test]
    fn git_push_short_flag_is_force_push() {
        assert!(is_force_push_command(&json!({
            "command": "git push origin HEAD -f"
        })));
    }

    #[test]
    fn git_push_force_with_lease_is_force_push() {
        assert!(is_force_push_command(&json!({
            "command": "git push origin HEAD --force-with-lease"
        })));
    }

    #[test]
    fn git_push_without_force_flag_is_not_force_push() {
        assert!(!is_force_push_command(&json!({
            "command": "git push origin HEAD"
        })));
    }

    #[test]
    fn jj_git_push_without_force_is_not_force_push() {
        assert!(!is_force_push_command(&json!({
            "command": "jj git push -b my-feature --allow-new"
        })));
    }

    #[test]
    fn non_push_command_is_not_force_push() {
        assert!(!is_force_push_command(&json!({
            "command": "gh pr create --head boss/exec_abc"
        })));
    }

    #[test]
    fn dash_f_inside_filename_does_not_trigger() {
        // A filename like `-f` is unlikely, but the token-split guard
        // means only a standalone `-f` matches; filenames with `-f` as
        // part of a longer string (e.g. `Makefile`) do not.
        assert!(!is_force_push_command(&json!({
            "command": "cat Makefile"
        })));
    }

    #[test]
    fn missing_command_field_returns_false() {
        assert!(!is_force_push_command(&json!({ "timeout": 30000 })));
    }

    #[test]
    fn null_tool_input_returns_false() {
        assert!(!is_force_push_command(&json!(null)));
    }

    // ── extract_resolution_comment_url ───────────────────────────

    #[test]
    fn extracts_issuecomment_url_from_stdout() {
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/463#issuecomment-4446168868\n",
            "stderr": "",
        });
        assert_eq!(
            extract_resolution_comment_url(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/463#issuecomment-4446168868"),
        );
    }

    #[test]
    fn falls_back_to_stderr_for_comment_url() {
        let response = json!({
            "stdout": "",
            "stderr": "https://github.com/spinyfin/mono/pull/463#issuecomment-9999\n",
        });
        assert_eq!(
            extract_resolution_comment_url(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/463#issuecomment-9999"),
        );
    }

    #[test]
    fn plain_pr_url_without_comment_anchor_is_not_a_comment_url() {
        // A bare PR URL emitted by `gh pr create` must NOT match — only
        // anchored issue-comment URLs count as the resolution-comment signal.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/463\n",
            "stderr": "",
        });
        assert_eq!(extract_resolution_comment_url(&response), None);
    }

    #[test]
    fn no_url_in_response_returns_none() {
        let response = json!({
            "stdout": "All conflicts resolved.\n",
            "stderr": "",
        });
        assert_eq!(extract_resolution_comment_url(&response), None);
    }

    // ── StagedResolutionSignalCache ───────────────────────────────

    #[test]
    fn cache_records_force_pushed_signal() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_abc", ResolutionSignal::ForcePushed);
        assert!(cache.has_any_signal("exec_abc"));
    }

    #[test]
    fn cache_records_comment_posted_signal() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_abc", ResolutionSignal::ResolutionCommentPosted);
        assert!(cache.has_any_signal("exec_abc"));
    }

    #[test]
    fn cache_accumulates_multiple_signals() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_abc", ResolutionSignal::ForcePushed);
        cache.record_signal("exec_abc", ResolutionSignal::ResolutionCommentPosted);
        assert!(cache.has_any_signal("exec_abc"));
    }

    #[test]
    fn cache_is_idempotent_per_signal_kind() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_abc", ResolutionSignal::ForcePushed);
        cache.record_signal("exec_abc", ResolutionSignal::ForcePushed);
        assert!(cache.has_any_signal("exec_abc"));
    }

    #[test]
    fn cache_isolates_executions() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_a", ResolutionSignal::ForcePushed);
        assert!(cache.has_any_signal("exec_a"));
        assert!(!cache.has_any_signal("exec_b"));
    }

    #[test]
    fn cache_has_no_signal_for_unknown_execution() {
        let cache = StagedResolutionSignalCache::new();
        assert!(!cache.has_any_signal("exec_unknown"));
    }

    #[test]
    fn cache_forget_drops_signals_and_makes_has_any_signal_false() {
        let cache = StagedResolutionSignalCache::new();
        cache.record_signal("exec_abc", ResolutionSignal::ForcePushed);
        cache.forget("exec_abc");
        assert!(!cache.has_any_signal("exec_abc"));
    }

    #[test]
    fn cache_forget_is_idempotent() {
        let cache = StagedResolutionSignalCache::new();
        cache.forget("never-staged");
        cache.forget("never-staged");
        assert!(!cache.has_any_signal("never-staged"));
    }
}
