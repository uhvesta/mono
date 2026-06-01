//! Shared classifier for `gh pr|issue <subcommand>` and `cube pr ensure`
//! Bash invocations.
//!
//! Two engine surfaces inspect the worker's Bash commands for `gh`
//! invocations:
//!
//! - [`crate::pr_url_capture`] — the PostToolUse path that stages the PR
//!   URL a worker's `gh pr create` / `view` / `edit` printed on stdout.
//! - [`crate::editorial_hook`] — the PreToolUse path that runs editorial
//!   rules over `gh pr|issue {create,edit,comment,review}` bodies.
//!
//! Both need the same first step: decide whether a command string is a
//! deliberate `gh pr` / `gh issue` invocation and, if so, which noun and
//! subcommand it targets. This module is that single source of truth so
//! the two paths can never drift on what counts as a `gh` call.
//!
//! The matcher tolerates the envelope workers actually emit — leading
//! `GIT_DIR=…`-style env assignments (jj-backed workspaces lack a
//! top-level `.git`, so `gh` is run with `GIT_DIR=.jj/repo/store/git`)
//! and the command appearing after a shell delimiter (`&&`, `;`, `|`).
//! It is the regex the editorial design names as the enforcement
//! envelope:
//!
//! ```text
//! ^\s*(GIT_DIR=\S+\s+)?gh\s+(pr|issue)\s+(create|edit|comment|review)\b
//! ```
//!
//! generalised here to capture *any* subcommand (so the PR-URL path can
//! ask for `view` / `list` too) and any number of env-assignment
//! prefixes.
//!
//! ## `cube pr ensure` coverage
//!
//! Workers are instructed to create PRs via `cube pr ensure` rather than
//! calling `gh pr create` directly. `cube pr ensure` resolves the repo
//! remote and then shells out to `gh pr create` internally — but that
//! subprocess is invisible to the PreToolUse hook, which only sees the
//! outer `cube pr ensure ...` command. [`is_cube_pr_ensure`] detects this
//! path so the editorial hook can enforce the same rules it applies to a
//! literal `gh pr create`.

use std::sync::LazyLock;

use regex::Regex;

/// Whether a `gh` invocation targets pull requests or issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhNoun {
    Pr,
    Issue,
}

/// A classified `gh pr|issue <subcommand>` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhInvocation {
    pub noun: GhNoun,
    /// The subcommand token, lowercased as written (`create`, `edit`,
    /// `comment`, `review`, `view`, `list`, …).
    pub subcommand: String,
}

/// Matches `gh pr|issue <subcommand>` anywhere a real command would put
/// it: at the start of the command, after any number of `VAR=value`
/// env-assignment prefixes, or after a shell delimiter. The leading
/// `(?:^|[\s;&|()])` anchor prevents `notgh pr create` style false
/// positives — `gh` must begin a token.
static GH_INVOCATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s;&|()])(?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+)*gh\s+(pr|issue)\s+([a-z][a-z-]*)",
    )
    .expect("gh invocation regex compiles")
});

/// Classify a Bash command string as a `gh pr` / `gh issue` invocation.
///
/// Returns `None` when the command is not a recognisable `gh pr` /
/// `gh issue` call (including non-`gh` commands, `gh` calls on other
/// nouns, and `gh pr`/`gh issue` with no subcommand). The first match in
/// the string wins, mirroring the substring semantics the PR-URL path
/// has always used.
pub fn classify(command: &str) -> Option<GhInvocation> {
    let caps = GH_INVOCATION_RE.captures(command)?;
    let noun = match &caps[1] {
        "pr" => GhNoun::Pr,
        "issue" => GhNoun::Issue,
        _ => return None,
    };
    Some(GhInvocation {
        noun,
        subcommand: caps[2].to_owned(),
    })
}

/// Matches `cube pr ensure` anywhere it would appear as a real command:
/// at the start, after env-var assignments, or after a shell delimiter.
static CUBE_PR_ENSURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s;&|()])(?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+)*cube\s+pr\s+ensure\b",
    )
    .expect("cube pr ensure regex compiles")
});

/// Returns `true` when `command` is a `cube pr ensure` invocation.
///
/// Workers are instructed to create PRs via `cube pr ensure` rather than
/// calling `gh pr create` directly. `cube` shells out to `gh pr create`
/// internally, making that call invisible to the PreToolUse hook. This
/// predicate lets the editorial hook intercept the outer `cube pr ensure`
/// command and apply the same checks it would apply to a `gh pr create`.
pub fn is_cube_pr_ensure(command: &str) -> bool {
    CUBE_PR_ENSURE_RE.is_match(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_gh_pr_create() {
        assert_eq!(
            classify("gh pr create --title 'x' --body 'y'"),
            Some(GhInvocation {
                noun: GhNoun::Pr,
                subcommand: "create".to_owned(),
            }),
        );
    }

    #[test]
    fn classifies_with_git_dir_prefix() {
        // jj-backed workspaces run gh with GIT_DIR set.
        assert_eq!(
            classify("GIT_DIR=.jj/repo/store/git gh pr edit 42 --body 'z'"),
            Some(GhInvocation {
                noun: GhNoun::Pr,
                subcommand: "edit".to_owned(),
            }),
        );
    }

    #[test]
    fn classifies_multiple_env_prefixes() {
        assert_eq!(
            classify("A=1 GIT_DIR=x gh issue comment 9 --body 'hi'"),
            Some(GhInvocation {
                noun: GhNoun::Issue,
                subcommand: "comment".to_owned(),
            }),
        );
    }

    #[test]
    fn classifies_gh_issue_create() {
        assert_eq!(
            classify("gh issue create --title t --body b").map(|i| (i.noun, i.subcommand)),
            Some((GhNoun::Issue, "create".to_owned())),
        );
    }

    #[test]
    fn classifies_pr_review_and_comment() {
        assert_eq!(classify("gh pr review --approve").unwrap().subcommand, "review");
        assert_eq!(classify("gh pr comment 3 -b hey").unwrap().subcommand, "comment");
    }

    #[test]
    fn classifies_after_shell_delimiter() {
        // A worker that chains `cd … && gh pr create` still classifies.
        let inv = classify("cd repo && gh pr create --body x").unwrap();
        assert_eq!(inv.noun, GhNoun::Pr);
        assert_eq!(inv.subcommand, "create");
    }

    #[test]
    fn leading_whitespace_is_tolerated() {
        assert_eq!(classify("   gh pr view").unwrap().subcommand, "view");
    }

    #[test]
    fn rejects_non_gh_command() {
        assert_eq!(classify("bossctl task show task_123"), None);
        assert_eq!(classify("cat chore.md"), None);
        assert_eq!(classify("grep -r 'pull/' . | head -5"), None);
    }

    #[test]
    fn rejects_gh_other_noun() {
        // `gh repo`, `gh release` are not pr/issue invocations.
        assert_eq!(classify("gh repo clone foo/bar"), None);
        assert_eq!(classify("gh release create v1"), None);
    }

    #[test]
    fn rejects_gh_pr_with_no_subcommand() {
        assert_eq!(classify("gh pr"), None);
        assert_eq!(classify("gh pr "), None);
    }

    #[test]
    fn rejects_token_ending_in_gh() {
        // `highgh pr` must not match — `gh` has to begin a token.
        assert_eq!(classify("highgh pr create"), None);
    }

    // --- is_cube_pr_ensure ---

    #[test]
    fn detects_cube_pr_ensure() {
        assert!(is_cube_pr_ensure("cube pr ensure --branch feat/foo --title 'My PR'"));
    }

    #[test]
    fn detects_cube_pr_ensure_after_shell_delimiter() {
        assert!(is_cube_pr_ensure("jj describe -m 'msg' && cube pr ensure --branch b"));
    }

    #[test]
    fn detects_cube_pr_ensure_with_env_prefix() {
        assert!(is_cube_pr_ensure("GIT_DIR=.git cube pr ensure --branch b"));
    }

    #[test]
    fn rejects_cube_pr_list() {
        // `cube pr list` is not ensure.
        assert!(!is_cube_pr_ensure("cube pr list"));
    }

    #[test]
    fn rejects_non_cube_command() {
        assert!(!is_cube_pr_ensure("gh pr create --title x"));
        assert!(!is_cube_pr_ensure("cube workspace lease mono"));
    }

    #[test]
    fn rejects_token_ending_in_cube() {
        // `notcube pr ensure` must not match.
        assert!(!is_cube_pr_ensure("notcube pr ensure --branch b"));
    }
}
