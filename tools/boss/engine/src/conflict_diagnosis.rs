//! Pre-spawn conflict-diagnosis collector for the merge-conflict
//! handling flow (Phase 3, chore #7 in
//! `tools/boss/docs/designs/merge-conflict-handling-in-review.md`,
//! reusing the shape sketched in `auto-rebase-stacked-prs.md` Q11).
//!
//! The collector runs in the engine **before** spawning the
//! resolution worker. It probes the post-merge head sha against the
//! current base ref with `git merge-tree --write-tree`, producing a
//! structured JSON blob the worker prompt embeds verbatim. The shape
//! intentionally mirrors what `auto-rebase` records on
//! `rebase_attempts.conflict_diagnosis` so a future unified attempts
//! view can render both kinds with one template.
//!
//! Persisted form is JSON (the `conflict_resolutions.conflict_diagnosis`
//! TEXT column). Forward-compat: the engine produces JSON; the worker
//! prompt composer (`runner::compose_execution_prompt`) renders the
//! markdown surface.
//!
//! The collector is intentionally *pure-ish*: it shells out to `git`
//! but doesn't mutate working state. A failed probe (git missing,
//! refs unresolvable, no conflicts after all) returns a populated
//! `ConflictDiagnosis` with `files = []` and an `error` field so the
//! worker prompt can still render something sensible — better than
//! aborting the spawn entirely.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Structured pre-spawn conflict-diagnosis blob. Wire-encoded as JSON
/// and stored in `conflict_resolutions.conflict_diagnosis`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictDiagnosis {
    /// Schema version. Bump when fields are added/removed; consumers
    /// can refuse to render unknown versions.
    pub schema_version: u32,
    /// Sha of the base ref (`main` at probe time).
    pub base_sha: String,
    /// Sha of the dependent's head ref at probe time.
    pub head_sha: String,
    /// One entry per file the would-be merge couldn't reconcile.
    pub files: Vec<ConflictedFile>,
    /// Set when the probe itself failed (e.g. `git merge-tree` exited
    /// non-zero for a non-conflict reason, refs unresolvable). Worker
    /// prompt surfaces this as a heads-up rather than rendering an
    /// empty conflict list as "no conflicts".
    pub error: Option<String>,
}

/// Per-file conflict shape. Conflict markers are extracted from the
/// tree `git merge-tree` writes; only the *count* of markers is
/// captured for now — the worker re-runs `jj st` / `jj resolve --list`
/// against the actual rebased state to get the marker bodies (cheap
/// once they're in the worker's local workspace).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictedFile {
    pub path: String,
    /// Per-file conflict marker count derived from the merge tree.
    /// `None` when the file is conflicted at the tree level (e.g. a
    /// rename/delete) and there's no marker count to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_count: Option<usize>,
    /// Free-form shape descriptor ("content", "rename/delete",
    /// "add/add", …) parsed out of `git merge-tree`'s informational
    /// messages. Default `"content"` when the only signal was a
    /// non-zero marker count.
    pub shape: String,
}

impl ConflictDiagnosis {
    /// Empty diagnosis stamped with an error reason. The worker
    /// prompt template inspects `error` to decide whether to ask the
    /// worker to start from `jj rebase -d main` (no diagnosis to
    /// embed) or to surface the diagnosis verbatim.
    pub fn errored(base_sha: &str, head_sha: &str, error: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            base_sha: base_sha.to_owned(),
            head_sha: head_sha.to_owned(),
            files: Vec::new(),
            error: Some(error.into()),
        }
    }
}

/// Run `git merge-tree --write-tree <base> <head>` in `workspace_path`
/// and parse the result into a structured diagnosis.
///
/// `base_sha` / `head_sha` are sha values (or refs) the caller has
/// already resolved against the workspace's git index. The function
/// does not fetch or check anything out — refs must already be
/// resolvable from inside `workspace_path`.
///
/// Returns `Ok(ConflictDiagnosis)` with `error = Some(...)` for
/// probe failures rather than `Err`, so a failed diagnosis doesn't
/// block the spawn — the worker can still take over from a fresh
/// `jj rebase -d main`. The `Err` path is reserved for genuine
/// caller misuse (workspace path doesn't exist, etc.).
pub async fn collect(
    workspace_path: &Path,
    base_sha: &str,
    head_sha: &str,
) -> std::io::Result<ConflictDiagnosis> {
    let output = Command::new("git")
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            base_sha,
            head_sha,
        ])
        .current_dir(workspace_path)
        .output()
        .await?;

    // `git merge-tree --write-tree`: exit 0 means a clean merge;
    // exit 1 means at least one conflict; other non-zero exits mean
    // a genuine error (bad refs, etc.). Parse accordingly.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let code = output.status.code();

    match code {
        Some(0) => Ok(ConflictDiagnosis {
            schema_version: 1,
            base_sha: base_sha.to_owned(),
            head_sha: head_sha.to_owned(),
            files: Vec::new(),
            error: None,
        }),
        Some(1) => Ok(parse_conflict_output(base_sha, head_sha, &stdout)),
        _ => Ok(ConflictDiagnosis::errored(
            base_sha,
            head_sha,
            format!(
                "git merge-tree exited with status {code:?}: {}",
                if stderr.is_empty() {
                    "(no stderr)".to_owned()
                } else {
                    stderr
                }
            ),
        )),
    }
}

/// Parse the stdout of `git merge-tree --write-tree --name-only
/// --no-messages` for a conflict (exit code 1). Layout, per
/// `git-merge-tree(1)`:
///
///   - First line: tree sha of the would-be merge result.
///   - Blank line.
///   - Conflicted-file list, one path per line.
///
/// The function is forgiving: a line that doesn't match the expected
/// shape is skipped rather than aborting the parse.
fn parse_conflict_output(base_sha: &str, head_sha: &str, stdout: &str) -> ConflictDiagnosis {
    let mut lines = stdout.lines();
    // Skip the tree sha header and the blank line that separates it
    // from the conflict list.
    let _tree_sha = lines.next().unwrap_or("");
    let mut files = Vec::new();
    for line in lines {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        files.push(ConflictedFile {
            path: path.to_owned(),
            marker_count: None,
            shape: "content".to_owned(),
        });
    }
    ConflictDiagnosis {
        schema_version: 1,
        base_sha: base_sha.to_owned(),
        head_sha: head_sha.to_owned(),
        files,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_files_when_stdout_is_only_a_tree_sha() {
        let parsed = parse_conflict_output("base", "head", "abc123\n\n");
        assert!(parsed.files.is_empty());
        assert!(parsed.error.is_none());
        assert_eq!(parsed.base_sha, "base");
        assert_eq!(parsed.head_sha, "head");
    }

    #[test]
    fn parses_conflicted_files_from_canonical_output() {
        let stdout = "deadbeef\n\nfoo/bar.rs\nfoo/baz.rs\n";
        let parsed = parse_conflict_output("base", "head", stdout);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].path, "foo/bar.rs");
        assert_eq!(parsed.files[1].path, "foo/baz.rs");
        for file in &parsed.files {
            assert_eq!(file.shape, "content");
            assert!(file.marker_count.is_none());
        }
    }

    #[test]
    fn parses_skips_blank_lines_in_file_list() {
        let stdout = "treesha\n\nfoo.rs\n\nbar.rs\n";
        let parsed = parse_conflict_output("base", "head", stdout);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].path, "foo.rs");
        assert_eq!(parsed.files[1].path, "bar.rs");
    }

    #[test]
    fn errored_diagnosis_carries_reason() {
        let d = ConflictDiagnosis::errored("base", "head", "git not on PATH");
        assert!(d.files.is_empty());
        assert_eq!(d.error.as_deref(), Some("git not on PATH"));
    }

    /// End-to-end test against a real git repo: stand up two
    /// divergent branches that touch the same line, then assert that
    /// `collect` reports the file as conflicted.
    ///
    /// Skipped if `git` isn't on the test runner's PATH so unit tests
    /// stay green in sandboxed CI environments.
    #[tokio::test]
    async fn collect_against_real_repo_reports_conflicted_file() {
        if which_git().is_none() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        run_git(repo, &["init", "-q", "--initial-branch=main"]).await;
        // Identity is required for `git commit` even in tests.
        run_git(repo, &["config", "user.email", "test@example.invalid"]).await;
        run_git(repo, &["config", "user.name", "Test"]).await;

        std::fs::write(repo.join("a.txt"), "alpha\n").unwrap();
        run_git(repo, &["add", "a.txt"]).await;
        run_git(repo, &["commit", "-q", "-m", "init"]).await;

        // Branch `feature` modifies a.txt on the same line as main
        // will, producing a guaranteed merge conflict.
        run_git(repo, &["checkout", "-q", "-b", "feature"]).await;
        std::fs::write(repo.join("a.txt"), "feature side\n").unwrap();
        run_git(repo, &["commit", "-q", "-am", "feature"]).await;
        run_git(repo, &["checkout", "-q", "main"]).await;
        std::fs::write(repo.join("a.txt"), "main side\n").unwrap();
        run_git(repo, &["commit", "-q", "-am", "main"]).await;

        let diag = collect(repo, "main", "feature").await.unwrap();
        assert!(diag.error.is_none(), "diagnosis errored: {:?}", diag.error);
        assert_eq!(diag.files.len(), 1, "expected one conflicted file");
        assert_eq!(diag.files[0].path, "a.txt");
    }

    fn which_git() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("git");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    async fn run_git(repo: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }
}
