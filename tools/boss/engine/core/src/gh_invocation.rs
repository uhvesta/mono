//! Utilities for working with the `gh` CLI: command classification and
//! subprocess spawn helpers.
//!
//! ## Command classification
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
//! subcommand it targets. [`classify`] is that single source of truth so
//! the two paths can never drift on what counts as a `gh` call.
//!
//! The matcher tolerates the envelope workers actually emit — leading
//! `GIT_DIR=…`-style env assignments (jj-backed workspaces lack a
//! top-level `.git`, so `gh` is run with `GIT_DIR=.jj/repo/store/git`)
//! and the command appearing after a shell delimiter (`&&`, `;`, `|`).
//!
//! ## `cube pr create` coverage
//!
//! Workers are instructed to create PRs via `cube pr create` rather than
//! calling `gh pr create` directly. `cube pr create` resolves the repo
//! remote and then shells out to `gh pr create` internally — but that
//! subprocess is invisible to the PreToolUse hook, which only sees the
//! outer `cube pr create ...` command. [`is_cube_pr_create`] detects this
//! path (and the deprecated `cube pr ensure` alias, which also creates) so
//! the editorial hook can enforce the same rules it applies to a literal
//! `gh pr create`.
//!
//! ## Subprocess spawn helpers
//!
//! [`gh_output`] and [`run_gh`] are re-exported from `boss_github::gh_runner`.
//! The implementation lives in the shared `boss-github` crate so both the
//! engine's direct `gh` shellouts (completion, merge polling, runner,
//! merge-when-ready, design detection) and the Contents helper share one
//! copy. Every call site spawns `gh` with the identical stdio envelope
//! (stdin null, stdout+stderr piped, `kill_on_drop(true)`); only the
//! post-spawn handling varies.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// Strip the CONTENT of single- and double-quoted strings from `cmd`,
/// preserving the quote characters themselves. This prevents phrases like
/// `cube pr ensure` that appear inside a quoted commit-message argument
/// (`-m "...cube pr ensure..."`) from matching the command-detection
/// regexes — only the actual invoked program and its verb/subcommand tokens
/// are visible after stripping.
///
/// Examples:
/// - `jj describe -m "cube pr ensure"` → `jj describe -m ""`
/// - `gh pr create --title "Foo"` → `gh pr create --title ""`
/// - `cube pr ensure --branch foo` → `cube pr ensure --branch foo` (unchanged)
fn strip_quoted_string_contents(cmd: &str) -> Cow<'_, str> {
    // Fast path: if the command has no quotes at all, return it as-is.
    if !cmd.contains('"') && !cmd.contains('\'') {
        return Cow::Borrowed(cmd);
    }
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '"' => {
                // Skip content until closing '"', honouring backslash escapes.
                // Use `while let` (not `for … by_ref()`) so we can call
                // `chars.next()` again inside the body without a double-borrow.
                while let Some(ch) = chars.next() {
                    if ch == '\\' {
                        chars.next(); // consume the escaped character
                    } else if ch == '"' {
                        out.push('"');
                        break;
                    }
                }
            }
            '\'' => {
                // POSIX single-quoted strings have no escape sequences inside.
                for ch in chars.by_ref() {
                    if ch == '\'' {
                        out.push('\'');
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Cow::Owned(out)
}

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
    Regex::new(r"(?:^|[\s;&|()])(?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+)*gh\s+(pr|issue)\s+([a-z][a-z-]*)")
        .expect("gh invocation regex compiles")
});

/// Classify a Bash command string as a `gh pr` / `gh issue` invocation.
///
/// Returns `None` when the command is not a recognisable `gh pr` /
/// `gh issue` call (including non-`gh` commands, `gh` calls on other
/// nouns, and `gh pr`/`gh issue` with no subcommand). The first match in
/// the string wins, mirroring the substring semantics the PR-URL path
/// has always used.
///
/// Quoted argument values (e.g. `-m "gh pr create"`) are stripped before
/// matching so that a PR-creation phrase inside a commit message or
/// `--body` string does not falsely classify the command as a `gh`
/// invocation.
pub fn classify(command: &str) -> Option<GhInvocation> {
    let stripped = strip_quoted_string_contents(command);
    let caps = GH_INVOCATION_RE.captures(&stripped)?;
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

/// Matches a PR-creating `cube pr` invocation anywhere it would appear as a
/// real command: at the start, after env-var assignments, or after a shell
/// delimiter. Covers `cube pr create` and the deprecated `cube pr ensure`
/// alias (both author a PR body); `cube pr update` is deliberately excluded
/// because it never creates and carries no body to audit.
static CUBE_PR_CREATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s;&|()])(?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+)*cube\s+pr\s+(?:create|ensure)\b")
        .expect("cube pr create regex compiles")
});

/// Returns `true` when `command` is a PR-creating `cube pr` invocation
/// (`cube pr create` or the deprecated `cube pr ensure` alias).
///
/// Workers are instructed to create PRs via `cube pr create` rather than
/// calling `gh pr create` directly. `cube` shells out to `gh pr create`
/// internally, making that call invisible to the PreToolUse hook. This
/// predicate lets the editorial hook intercept the outer `cube pr create`
/// command and apply the same checks it would apply to a `gh pr create`.
///
/// Quoted argument values are stripped before matching so that a commit
/// message mentioning `cube pr create` does not produce a false positive.
pub fn is_cube_pr_create(command: &str) -> bool {
    let stripped = strip_quoted_string_contents(command);
    CUBE_PR_CREATE_RE.is_match(&stripped)
}

/// Fast pre-filter for the editorial PreToolUse audit path. Returns `true`
/// when `command` could be a `gh pr|issue {create,edit,comment,review}` or
/// PR-creating `cube pr {create,ensure}` invocation — the two surfaces the
/// editorial hook covers.
///
/// This is a cheap substring check; the heavier [`classify`] /
/// [`is_cube_pr_create`] parsing follows only when this returns `true`.
pub fn is_editorial_candidate(command: &str) -> bool {
    (command.contains("gh ") && (command.contains(" pr ") || command.contains(" issue ")))
        || (command.contains("cube ")
            && command.contains(" pr ")
            && (command.contains("create") || command.contains("ensure")))
}

// ── Subprocess spawn helpers ──────────────────────────────────────────────────
//
// The implementation lives in the shared `boss-github` crate so engine and
// the Contents helper share one copy. Re-exported here as `pub(crate)` so
// all existing call sites within `boss-engine` need no changes.
pub(crate) use boss_github::gh_runner::{gh_output, run_gh};

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

    // --- is_cube_pr_create ---

    #[test]
    fn detects_cube_pr_create() {
        assert!(is_cube_pr_create("cube pr create --branch feat/foo --title 'My PR'"));
    }

    #[test]
    fn detects_deprecated_cube_pr_ensure() {
        // The deprecated alias still create-or-reuses, so it must still be
        // intercepted for editorial enforcement.
        assert!(is_cube_pr_create("cube pr ensure --branch feat/foo --title 'My PR'"));
    }

    #[test]
    fn rejects_cube_pr_update() {
        // `cube pr update` never creates and carries no body — not a create.
        assert!(!is_cube_pr_create("cube pr update --branch feat/foo"));
    }

    #[test]
    fn detects_cube_pr_create_after_shell_delimiter() {
        assert!(is_cube_pr_create("jj describe -m 'msg' && cube pr create --branch b"));
    }

    #[test]
    fn detects_cube_pr_create_with_env_prefix() {
        assert!(is_cube_pr_create("GIT_DIR=.git cube pr create --branch b"));
    }

    #[test]
    fn rejects_cube_pr_list() {
        // `cube pr list` is not create.
        assert!(!is_cube_pr_create("cube pr list"));
    }

    #[test]
    fn rejects_non_cube_command() {
        assert!(!is_cube_pr_create("gh pr create --title x"));
        assert!(!is_cube_pr_create("cube workspace lease mono"));
    }

    #[test]
    fn rejects_token_ending_in_cube() {
        // `notcube pr create` must not match.
        assert!(!is_cube_pr_create("notcube pr create --branch b"));
    }

    // ── false-positive regression tests ──────────────────────────────────
    //
    // These guard against the bug where a PR-creation phrase inside a
    // quoted argument (e.g. a commit message) caused a false positive.

    #[test]
    fn cube_pr_create_not_matched_inside_double_quoted_commit_message() {
        // Reproduces the T1031 bug: the phrase appears inside -m "..."
        // but the command is `jj describe`, not `cube pr create`.
        assert!(
            !is_cube_pr_create(
                r#"jj describe -m "fix(boss-engine): extend editorial hook to intercept cube pr create""#,
            ),
            "cube pr create inside a double-quoted commit message must not match",
        );
    }

    #[test]
    fn cube_pr_create_not_matched_inside_single_quoted_commit_message() {
        assert!(
            !is_cube_pr_create("jj describe -m 'fix: intercept cube pr create'"),
            "cube pr create inside a single-quoted commit message must not match",
        );
    }

    #[test]
    fn gh_pr_create_not_matched_inside_quoted_commit_message() {
        assert!(
            classify(r#"git commit -m "docs: explain gh pr create usage""#).is_none(),
            "gh pr create inside a quoted commit message must not classify as a gh invocation",
        );
    }

    #[test]
    fn cube_pr_create_still_matches_after_quoted_arg() {
        // A real `cube pr create` that happens to follow a quoted argument
        // (e.g. `jj describe -m "msg" && cube pr create`) must still be
        // caught. Stripping quotes must not suppress the real command.
        assert!(
            is_cube_pr_create(r#"jj describe -m "push fixes" && cube pr create --branch b"#),
            "cube pr create after a quoted arg must still match",
        );
    }

    #[test]
    fn gh_pr_create_still_matches_after_quoted_arg() {
        assert_eq!(
            classify(r#"jj describe -m "msg" && gh pr create --title "x""#).map(|i| i.subcommand),
            Some("create".to_owned()),
            "gh pr create after a quoted arg must still be classified",
        );
    }

    // ── strip_quoted_string_contents unit tests ───────────────────────────

    #[test]
    fn strip_removes_double_quoted_content() {
        assert_eq!(
            strip_quoted_string_contents(r#"foo -m "hello world" bar"#),
            r#"foo -m "" bar"#,
        );
    }

    #[test]
    fn strip_removes_single_quoted_content() {
        assert_eq!(
            strip_quoted_string_contents("foo -m 'hello world' bar"),
            "foo -m '' bar",
        );
    }

    #[test]
    fn strip_handles_backslash_escape_in_double_quotes() {
        // `\"` inside a double-quoted string does not end the string.
        assert_eq!(strip_quoted_string_contents(r#"cmd "a\"b" rest"#), r#"cmd "" rest"#,);
    }

    #[test]
    fn strip_preserves_unquoted_content() {
        let s = "cube pr ensure --branch foo";
        assert_eq!(strip_quoted_string_contents(s), s);
    }

    #[test]
    fn strip_fast_path_no_quotes() {
        // No allocation when there are no quotes.
        let s = "plain command arg1 arg2";
        let result = strip_quoted_string_contents(s);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }
}
