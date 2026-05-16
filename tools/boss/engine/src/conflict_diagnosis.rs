//! Pre-spawn conflict-diagnosis collector for the merge-conflict
//! handling flow (Phase 3, chore #7 in
//! `tools/boss/docs/designs/merge-conflict-handling-in-review.md`,
//! reusing the shape sketched in `auto-rebase-stacked-prs.md` Q11).
//!
//! The collector runs in the engine **before** spawning the
//! resolution worker. It probes the post-merge head sha against the
//! current base ref with `git merge-tree`, producing a
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
//!
//! ## Git version compatibility
//!
//! `git merge-tree --write-tree` (new structured form, exit 1 on conflict)
//! was added in git 2.38. Older git only supports the legacy three-argument
//! form `git merge-tree <base-tree> <branch1> <branch2>`, which always exits
//! 0 and embeds conflict markers in stdout. We detect the running git version
//! at probe time and choose the matching invocation.

use std::path::{Path, PathBuf};

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

/// Parse a `git --version` output string into a `(major, minor, patch)` tuple.
///
/// Accepts the canonical `git version X.Y.Z` prefix; extra vendor suffixes
/// (e.g. `(Apple Git-145)`) are ignored.
pub fn parse_git_version(version_output: &str) -> Option<(u32, u32, u32)> {
    let ver = version_output
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = ver.split('.').filter_map(|p| p.parse::<u32>().ok());
    Some((parts.next()?, parts.next()?, parts.next().unwrap_or(0)))
}

/// Returns true if `(major, minor, patch)` is new enough to support
/// `git merge-tree --write-tree` (added in git 2.38).
pub fn version_supports_write_tree(ver: (u32, u32, u32)) -> bool {
    (ver.0, ver.1) >= (2, 38)
}

/// Query the running `git` binary's version. Returns `None` when `git`
/// is not on PATH or when its output cannot be parsed.
async fn git_version() -> Option<(u32, u32, u32)> {
    let output = Command::new("git").arg("--version").output().await.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_version(stdout.trim())
}

/// Resolve the git directory for `workspace_path`.
///
/// For jj-only cube workspaces (no top-level `.git`), the real git
/// store is recorded in `.jj/repo/store/git_target` as a relative path
/// from `.jj/repo/store/`. This function reads that file and returns the
/// canonicalised absolute path so callers can pass it as `GIT_DIR`.
///
/// Falls back to `<workspace_path>/.git` when `git_target` is absent,
/// which covers colocated jj+git workspaces (test fixtures, dev-mode).
fn resolve_git_dir(workspace_path: &Path) -> std::io::Result<PathBuf> {
    let git_target_file = workspace_path
        .join(".jj")
        .join("repo")
        .join("store")
        .join("git_target");

    if git_target_file.exists() {
        let raw = std::fs::read_to_string(&git_target_file)?;
        let relative = raw.trim();
        let base = workspace_path.join(".jj").join("repo").join("store");
        let resolved = base.join(relative).canonicalize()?;
        Ok(resolved)
    } else {
        // Colocated jj+git workspace — the `.git` directory is at the root.
        Ok(workspace_path.join(".git"))
    }
}

/// Run `git merge-tree` in `workspace_path` and parse the result into a
/// structured diagnosis. Automatically selects the new `--write-tree` form
/// (git ≥ 2.38) or the legacy three-argument form based on the running git
/// binary's version.
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
    // Cube workspaces are jj-only (no top-level `.git`); git must be
    // told where the store is via GIT_DIR, otherwise it fails with
    // "not a git repository" during directory discovery.
    let git_dir = match resolve_git_dir(workspace_path) {
        Ok(p) => p,
        Err(e) => {
            return Ok(ConflictDiagnosis::errored(
                base_sha,
                head_sha,
                format!("could not resolve git dir: {e}"),
            ));
        }
    };

    let use_new_syntax = git_version()
        .await
        .map(version_supports_write_tree)
        // If version is unknown, assume new syntax (preserves prior behaviour on
        // platforms where `git --version` might be unusual but git is recent).
        .unwrap_or(true);

    if use_new_syntax {
        collect_new_syntax(workspace_path, &git_dir, base_sha, head_sha).await
    } else {
        collect_legacy(workspace_path, &git_dir, base_sha, head_sha).await
    }
}

/// New-syntax path: `git merge-tree --write-tree --name-only --no-messages
/// <base> <head>`. Requires git ≥ 2.38. Exit 0 = clean; exit 1 = conflicts.
async fn collect_new_syntax(
    workspace_path: &Path,
    git_dir: &Path,
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
        .env("GIT_DIR", git_dir)
        .output()
        .await?;

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
        Some(1) => Ok(parse_new_syntax_output(base_sha, head_sha, &stdout)),
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

/// Legacy-syntax path: compute merge base, then run
/// `git merge-tree <base-tree> <base> <head>`. Works on any git ≥ 2.0.
/// Exit is always 0; conflicts are signalled by `<<<<<<<` markers in stdout.
async fn collect_legacy(
    workspace_path: &Path,
    git_dir: &Path,
    base_sha: &str,
    head_sha: &str,
) -> std::io::Result<ConflictDiagnosis> {
    // The legacy form requires the common ancestor tree as first argument.
    let mb_output = Command::new("git")
        .args(["merge-base", base_sha, head_sha])
        .current_dir(workspace_path)
        .env("GIT_DIR", git_dir)
        .output()
        .await?;

    if !mb_output.status.success() {
        let stderr = String::from_utf8_lossy(&mb_output.stderr).trim().to_owned();
        return Ok(ConflictDiagnosis::errored(
            base_sha,
            head_sha,
            format!("git merge-base failed: {stderr}"),
        ));
    }

    let merge_base = String::from_utf8_lossy(&mb_output.stdout)
        .trim()
        .to_owned();

    let output = Command::new("git")
        .args(["merge-tree", &merge_base, base_sha, head_sha])
        .current_dir(workspace_path)
        .env("GIT_DIR", git_dir)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Ok(ConflictDiagnosis::errored(
            base_sha,
            head_sha,
            format!(
                "git merge-tree (legacy) exited with status {:?}: {}",
                output.status.code(),
                if stderr.is_empty() { "(no stderr)" } else { &stderr }
            ),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_legacy_output(base_sha, head_sha, &stdout))
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
fn parse_new_syntax_output(base_sha: &str, head_sha: &str, stdout: &str) -> ConflictDiagnosis {
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

/// Parse the stdout of the legacy `git merge-tree <base-tree> <b1> <b2>` form.
///
/// The output consists of sections (one per changed path), each starting with
/// a change-type header line, followed by indented file-info lines
/// (`base`/`our`/`their` with mode, sha, and path), then a unified diff.
/// Conflicting sections contain `<<<<<<<` markers in their diff portion.
///
/// Strategy: scan for `<<<<<<<` marker lines; for each, walk backwards to find
/// the nearest file-info line and extract the path from it.
fn parse_legacy_output(base_sha: &str, head_sha: &str, stdout: &str) -> ConflictDiagnosis {
    if !stdout.contains("<<<<<<<") {
        return ConflictDiagnosis {
            schema_version: 1,
            base_sha: base_sha.to_owned(),
            head_sha: head_sha.to_owned(),
            files: Vec::new(),
            error: None,
        };
    }

    let lines: Vec<&str> = stdout.lines().collect();
    let mut conflicted_files: Vec<String> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("<<<<<<<") {
            continue;
        }
        // Walk backwards from this conflict marker to find the nearest
        // file-info line: "  base   <mode> <sha> <path>"
        // (also accepts "our" and "their" as the role word).
        for j in (0..i).rev() {
            let parts: Vec<&str> = lines[j].split_whitespace().collect();
            if parts.len() >= 4 && matches!(parts[0], "base" | "our" | "their") {
                // Rejoin everything after mode+sha as the path (handles spaces).
                let filename = parts[3..].join(" ");
                if !conflicted_files.contains(&filename) {
                    conflicted_files.push(filename);
                }
                break;
            }
        }
    }

    let files = conflicted_files
        .into_iter()
        .map(|path| ConflictedFile {
            path,
            marker_count: None,
            shape: "content".to_owned(),
        })
        .collect();

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

    // --- parse_git_version ---------------------------------------------------

    #[test]
    fn parses_standard_version_string() {
        assert_eq!(
            parse_git_version("git version 2.39.3"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parses_apple_git_suffix() {
        assert_eq!(
            parse_git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39, 5))
        );
    }

    #[test]
    fn parses_two_part_version() {
        assert_eq!(parse_git_version("git version 2.38"), Some((2, 38, 0)));
    }

    #[test]
    fn returns_none_for_garbage() {
        assert_eq!(parse_git_version("not a version string"), None);
    }

    // --- version_supports_write_tree -----------------------------------------

    #[test]
    fn version_boundary_correct() {
        assert!(!version_supports_write_tree((2, 37, 99)));
        assert!(version_supports_write_tree((2, 38, 0)));
        assert!(version_supports_write_tree((2, 48, 0)));
        assert!(version_supports_write_tree((3, 0, 0)));
    }

    // --- parse_new_syntax_output (formerly parse_conflict_output) ------------

    #[test]
    fn parses_no_files_when_stdout_is_only_a_tree_sha() {
        let parsed = parse_new_syntax_output("base", "head", "abc123\n\n");
        assert!(parsed.files.is_empty());
        assert!(parsed.error.is_none());
        assert_eq!(parsed.base_sha, "base");
        assert_eq!(parsed.head_sha, "head");
    }

    #[test]
    fn parses_conflicted_files_from_canonical_output() {
        let stdout = "deadbeef\n\nfoo/bar.rs\nfoo/baz.rs\n";
        let parsed = parse_new_syntax_output("base", "head", stdout);
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
        let parsed = parse_new_syntax_output("base", "head", stdout);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].path, "foo.rs");
        assert_eq!(parsed.files[1].path, "bar.rs");
    }

    // --- parse_legacy_output -------------------------------------------------

    #[test]
    fn legacy_no_conflict_markers_returns_empty() {
        let stdout = "changed in both\n  base   100644 abc a.txt\n  our    100644 def a.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let parsed = parse_legacy_output("base", "head", stdout);
        assert!(parsed.files.is_empty());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn legacy_conflict_extracts_filename() {
        // Simulate `git merge-tree <base> main feature` output for a.txt conflict.
        let stdout = "\
changed in both\n\
  base   100644 aaa000 a.txt\n\
  our    100644 bbb111 a.txt\n\
  their  100644 ccc222 a.txt\n\
@@ -1,1 +1,1 @@\n\
<<<<<<< .our\n\
main side\n\
||||||| .base\n\
alpha\n\
=======\n\
feature side\n\
>>>>>>> .their\n";
        let parsed = parse_legacy_output("base", "head", stdout);
        assert_eq!(parsed.files.len(), 1, "expected one conflicted file");
        assert_eq!(parsed.files[0].path, "a.txt");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn legacy_deduplicates_same_file_multiple_conflict_markers() {
        let stdout = "\
changed in both\n\
  base   100644 aaa a.txt\n\
  our    100644 bbb a.txt\n\
  their  100644 ccc a.txt\n\
@@ -1,1 +1,1 @@\n\
<<<<<<< .our\n\
hunk1 our\n\
=======\n\
hunk1 their\n\
>>>>>>> .their\n\
<<<<<<< .our\n\
hunk2 our\n\
=======\n\
hunk2 their\n\
>>>>>>> .their\n";
        let parsed = parse_legacy_output("base", "head", stdout);
        assert_eq!(parsed.files.len(), 1, "should deduplicate");
        assert_eq!(parsed.files[0].path, "a.txt");
    }

    // --- errored_diagnosis ---------------------------------------------------

    #[test]
    fn errored_diagnosis_carries_reason() {
        let d = ConflictDiagnosis::errored("base", "head", "git not on PATH");
        assert!(d.files.is_empty());
        assert_eq!(d.error.as_deref(), Some("git not on PATH"));
    }

    // --- git version probe ---------------------------------------------------

    /// Assert that the running `git` binary's version is parseable and that
    /// the invocation shape we select for it is internally consistent. This
    /// catches configuration drift where the git binary changes but the version
    /// detection logic doesn't keep up.
    #[tokio::test]
    async fn running_git_version_is_parseable_and_supported() {
        if which_git().is_none() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let ver = git_version().await;
        assert!(
            ver.is_some(),
            "git --version output could not be parsed; check parse_git_version()"
        );
        let (major, minor, patch) = ver.unwrap();
        // git 2.28 added --initial-branch; our test infrastructure relies on it.
        assert!(
            (major, minor) >= (2, 28),
            "git {major}.{minor}.{patch} is too old; test infra requires git ≥ 2.28"
        );
        // Log which invocation shape will be used, so CI logs are self-documenting.
        if version_supports_write_tree((major, minor, patch)) {
            eprintln!("git {major}.{minor}.{patch}: using --write-tree (new syntax)");
        } else {
            eprintln!("git {major}.{minor}.{patch}: using legacy <base-tree> form");
        }
    }

    // --- end-to-end against a real git repo ----------------------------------

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

    /// Regression test: `collect` must succeed when the workspace is
    /// jj-only (no top-level `.git`). We simulate a cube workspace by
    /// building a normal git repo, then creating a separate jj-style
    /// workspace dir that has `.jj/repo/store/git_target` pointing at
    /// the repo's `.git`. There is intentionally no `.git` at the
    /// workspace root — that's the shape that was producing the
    /// "not a git repository" error.
    #[tokio::test]
    async fn collect_against_jj_only_workspace_resolves_git_dir() {
        if which_git().is_none() {
            eprintln!("skipping: git not on PATH");
            return;
        }

        // --- Build a normal git repo with a conflict between main/feature --
        let git_dir = tempfile::tempdir().unwrap();
        let git_repo = git_dir.path();
        run_git(git_repo, &["init", "-q", "--initial-branch=main"]).await;
        run_git(git_repo, &["config", "user.email", "test@example.invalid"]).await;
        run_git(git_repo, &["config", "user.name", "Test"]).await;

        std::fs::write(git_repo.join("a.txt"), "alpha\n").unwrap();
        run_git(git_repo, &["add", "a.txt"]).await;
        run_git(git_repo, &["commit", "-q", "-m", "init"]).await;

        run_git(git_repo, &["checkout", "-q", "-b", "feature"]).await;
        std::fs::write(git_repo.join("a.txt"), "feature side\n").unwrap();
        run_git(git_repo, &["commit", "-q", "-am", "feature"]).await;

        run_git(git_repo, &["checkout", "-q", "main"]).await;
        std::fs::write(git_repo.join("a.txt"), "main side\n").unwrap();
        run_git(git_repo, &["commit", "-q", "-am", "main"]).await;

        // --- Build a jj-style workspace (no .git at root) -----------------
        // git_target is an absolute path here; jj itself uses relative paths
        // but our resolver just joins and canonicalises, so absolute is fine
        // for the test.
        let jj_ws = tempfile::tempdir().unwrap();
        let ws = jj_ws.path();
        let store_path = ws.join(".jj").join("repo").join("store");
        std::fs::create_dir_all(&store_path).unwrap();
        std::fs::write(
            store_path.join("git_target"),
            git_repo.join(".git").to_str().unwrap(),
        )
        .unwrap();
        // Intentionally no .git at ws root — this is the jj-only shape.

        let diag = collect(ws, "main", "feature").await.unwrap();
        assert!(
            diag.error.is_none(),
            "jj-only workspace: diagnosis errored: {:?}",
            diag.error
        );
        assert_eq!(
            diag.files.len(),
            1,
            "jj-only workspace: expected one conflicted file, got: {:?}",
            diag.files
        );
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
