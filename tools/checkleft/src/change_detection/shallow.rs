use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use tracing::info;

use crate::vcs::VcsKind;

use super::scenario::Scenario;

/// Successive `--deepen=<N>` increments tried before falling back to full
/// `--unshallow`. Each step adds N commits to the existing shallow depth.
const DEEPEN_LADDER: &[u32] = &[50, 250, 1000];

/// Ensure the local git repo has enough history to compute a base for
/// `needed_ref`.
///
/// # Behaviour
///
/// - Not shallow → returns `Ok(())` immediately.
/// - Merge-queue/push scenarios only need `HEAD^1`; a single
///   `--deepen=1` is sufficient and no reachability re-test is needed.
/// - PR/branch/local scenarios need the merge-base against `needed_ref`;
///   the bounded ladder `--deepen=50 → 250 → 1000` is tried with a
///   reachability check after each step, then `--unshallow` as last resort.
/// - If the base is **still unreachable** after all attempts, returns a
///   precise, actionable error. **Never silently falls back to the repo tip** —
///   that silent mis-scoping is the exact failure mode this project exists to
///   prevent.
///
/// # jj colocated repos
///
/// jj's colocated git repo is what is shallow. We operate on the underlying
/// git repo via the `.git` directory at the workspace root and then run
/// `jj git import` to sync jj's op log after deepening.
pub fn ensure_history(root: &Path, kind: VcsKind, needed_ref: &str, scenario: &Scenario) -> Result<()> {
    let git_root = resolve_git_root(root, kind)?;

    if !is_shallow(&git_root)? {
        return Ok(());
    }

    info!(needed_ref, "repo is shallow; deepening history");

    match scenario {
        // ── Merge-queue / push-to-default: only HEAD^1 is needed ─────────────
        // A single --deepen=1 brings the parent commit into local history.
        // No merge-base computation is required for these scenarios.
        Scenario::MergeQueue | Scenario::PushToDefault => {
            deepen(&git_root, 1)?;
            info!("deepened by 1 to reach HEAD^1 for merge-queue/push-to-default scenario");
        }

        // ── PR / push-to-branch / local: need the merge-base against needed_ref
        // Try the bounded ladder then full unshallow before erroring.
        // PushToBranch needs merge-base(origin/<default_branch>, HEAD), so it
        // requires the same treatment as PR and Local — not just HEAD^1.
        Scenario::PullRequest { .. } | Scenario::PushToBranch { .. } | Scenario::Local => {
            let reached = deepen_until_reachable(&git_root, needed_ref)?;
            if !reached {
                bail!(
                    "base ref `{needed_ref}` is unreachable even after unshallowing the \
                     repository.\n\
                     Tried: --deepen={}, --deepen={}, --deepen={}, --unshallow\n\
                     The base branch may not have been fetched. Run:\n\
                     \n    git fetch origin {}\n\n\
                     then re-run checkleft.",
                    DEEPEN_LADDER[0],
                    DEEPEN_LADDER[1],
                    DEEPEN_LADDER[2],
                    strip_origin_prefix(needed_ref),
                );
            }
        }
    }

    // In jj colocated repos: sync jj's op log so jj commands see the deepened
    // history. See design Q3 — jj git import is needed after git-level fetches.
    if kind == VcsKind::Jujutsu {
        import_jj_git(&git_root)?;
    }

    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Resolve the git working directory to run shallow operations in.
///
/// - `Git` repos: returns `root` as-is.
/// - `Jujutsu` repos: the workspace is a jj workspace; the colocated `.git`
///   dir at `root` is the git repo. If no `.git` exists, we cannot deepen.
fn resolve_git_root(root: &Path, kind: VcsKind) -> Result<PathBuf> {
    match kind {
        VcsKind::Git => Ok(root.to_owned()),
        VcsKind::Jujutsu => {
            if root.join(".git").exists() {
                Ok(root.to_owned())
            } else {
                bail!(
                    "shallow history deepening requires a git-colocated jj workspace, \
                     but no `.git` directory was found at `{}`.\n\
                     Ensure the jj workspace was created with `jj git init --colocate`.",
                    root.display()
                );
            }
        }
    }
}

/// `true` if the repository is a shallow clone.
fn is_shallow(root: &Path) -> Result<bool> {
    let out = git_output(root, &["rev-parse", "--is-shallow-repository"])?;
    Ok(out.trim() == "true")
}

/// `true` if `git merge-base <base_ref> HEAD` exits 0, meaning the common
/// ancestor between `base_ref` and `HEAD` is present in the local history.
fn is_merge_base_reachable(root: &Path, base_ref: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["merge-base", base_ref, "HEAD"])
        .current_dir(root)
        .output()?
        .status;
    Ok(status.success())
}

/// Deepen by N additional commits: `git fetch --deepen=<n> origin`.
fn deepen(root: &Path, n: u32) -> Result<()> {
    let depth_arg = format!("--deepen={n}");
    run_git(root, &["fetch", &depth_arg, "origin"])
}

/// Full unshallow: `git fetch --unshallow origin`.
fn unshallow(root: &Path) -> Result<()> {
    run_git(root, &["fetch", "--unshallow", "origin"])
}

/// `jj git import` — sync jj's op log after deepening the underlying git repo.
fn import_jj_git(root: &Path) -> Result<()> {
    let output = Command::new("jj")
        .args(["git", "import"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`jj git import` failed after history deepening: {}",
            stderr.trim()
        );
    }
    info!("ran `jj git import` to sync after history deepening");
    Ok(())
}

/// Try the deepen ladder then full unshallow until `git merge-base <base_ref>
/// HEAD` succeeds. Returns `true` if reachable, `false` if not (even after
/// unshallow).
fn deepen_until_reachable(root: &Path, base_ref: &str) -> Result<bool> {
    for &step in DEEPEN_LADDER {
        deepen(root, step)?;
        info!(step, "deepened; re-checking merge-base reachability");
        if is_merge_base_reachable(root, base_ref)? {
            info!(base_ref, "merge-base is now reachable");
            return Ok(true);
        }
        // If the deepen step brought us to a complete (non-shallow) repo — e.g.
        // a small repo where the deepen budget exceeded the total commit count —
        // there is no further history to fetch. Skip --unshallow (which would
        // fail with "complete repository") and check reachability one final time.
        if !is_shallow(root)? {
            return is_merge_base_reachable(root, base_ref);
        }
    }

    // Last resort: full unshallow.
    info!("deepen ladder exhausted; unshallowing fully");
    unshallow(root)?;
    is_merge_base_reachable(root, base_ref)
}

/// Strip `origin/` prefix from a ref name for use in the user-facing remedy
/// message. `origin/main` → `main`; `main` → `main`.
fn strip_origin_prefix(r: &str) -> &str {
    r.strip_prefix("origin/").unwrap_or(r)
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("git output was not valid utf-8: {e}"))
}

fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    // ── Git helpers for test repos ────────────────────────────────────────────

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_ok(root: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn configure_git(root: &Path) {
        git(root, &["config", "user.email", "test@checkleft.example"]);
        git(root, &["config", "user.name", "Checkleft Test"]);
    }

    fn commit_file(root: &Path, name: &str, content: &str, msg: &str) {
        fs::write(root.join(name), content).expect("write file");
        git(root, &["add", name]);
        git(root, &["commit", "-m", msg]);
    }

    /// Build a "remote" repo with `commit_count` commits on `main`, then
    /// return a shallow clone of it at depth 1.
    ///
    /// Uses `git init + remote + fetch --depth=1` rather than `git clone
    /// --depth=1`, because git ignores `--depth` for local-transport clones
    /// (file:// path without an explicit protocol), producing a full clone.
    fn make_shallow_clone(commit_count: usize) -> (tempfile::TempDir, tempfile::TempDir) {
        let remote_dir = tempdir().expect("tempdir remote");
        let remote = remote_dir.path();

        git(remote, &["init", "-b", "main"]);
        configure_git(remote);

        for i in 0..commit_count {
            commit_file(remote, "f.txt", &format!("v{i}\n"), &format!("commit {i}"));
        }

        let clone_dir = tempdir().expect("tempdir clone");
        let clone = clone_dir.path();

        git(clone, &["init", "-b", "main"]);
        configure_git(clone);
        git(clone, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(clone, &["fetch", "--depth=1", "origin", "main"]);
        // Check out the fetched commit so HEAD is valid.
        git(clone, &["checkout", "-b", "main", "FETCH_HEAD"]);

        (remote_dir, clone_dir)
    }

    // ── strip_origin_prefix ───────────────────────────────────────────────────

    #[test]
    fn strip_origin_prefix_removes_origin_slash() {
        assert_eq!(strip_origin_prefix("origin/main"), "main");
        assert_eq!(strip_origin_prefix("origin/develop"), "develop");
    }

    #[test]
    fn strip_origin_prefix_passes_through_bare_branch() {
        assert_eq!(strip_origin_prefix("main"), "main");
        assert_eq!(strip_origin_prefix("feature/foo"), "feature/foo");
    }

    // ── is_shallow ────────────────────────────────────────────────────────────

    #[test]
    fn is_shallow_returns_false_for_full_repo() {
        let dir = tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        configure_git(dir.path());
        commit_file(dir.path(), "a.txt", "a\n", "initial");

        assert!(!is_shallow(dir.path()).expect("is_shallow"));
    }

    #[test]
    fn is_shallow_returns_true_for_shallow_clone() {
        let (_remote, clone) = make_shallow_clone(3);
        assert!(is_shallow(clone.path()).expect("is_shallow"));
    }

    // ── ensure_history: non-shallow no-op ────────────────────────────────────

    #[test]
    fn ensure_history_is_noop_when_not_shallow() {
        let dir = tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        configure_git(dir.path());
        commit_file(dir.path(), "a.txt", "a\n", "initial");

        // A non-existent ref would fail if the function tried to fetch/check.
        // Since the repo is not shallow, ensure_history returns Ok immediately.
        let result = ensure_history(
            dir.path(),
            VcsKind::Git,
            "nonexistent-ref",
            &Scenario::PullRequest {
                base_branch: "main".to_owned(),
            },
        );
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    // ── ensure_history: merge-queue/push deepens by 1 ────────────────────────

    #[test]
    fn ensure_history_deepens_by_one_for_merge_queue() {
        let (remote_dir, clone) = make_shallow_clone(5);
        let clone_path = clone.path();

        // Point origin at the local remote.
        // (The clone already has origin set to the remote path by `git clone`.)
        assert!(
            is_shallow(clone_path).expect("is_shallow"),
            "clone must start shallow"
        );

        // After ensure_history with MergeQueue, HEAD^1 must be reachable.
        ensure_history(clone_path, VcsKind::Git, "HEAD^1", &Scenario::MergeQueue)
            .expect("ensure_history MergeQueue");

        assert!(
            git_ok(clone_path, &["rev-parse", "HEAD^1"]),
            "HEAD^1 must be reachable after deepen=1"
        );

        drop(remote_dir);
    }

    #[test]
    fn ensure_history_deepens_by_one_for_push_to_default() {
        let (remote_dir, clone) = make_shallow_clone(5);
        let clone_path = clone.path();

        ensure_history(clone_path, VcsKind::Git, "HEAD^1", &Scenario::PushToDefault)
            .expect("ensure_history PushToDefault");

        assert!(git_ok(clone_path, &["rev-parse", "HEAD^1"]));
        drop(remote_dir);
    }

    // ── ensure_history: PR scenario deepens until merge-base reachable ────────

    #[test]
    fn ensure_history_deepens_until_merge_base_reachable_for_pr() {
        // Build a remote with: main has 5 commits, then a branch with 1 commit.
        let remote_dir = tempdir().expect("tempdir remote");
        let remote = remote_dir.path();
        git(remote, &["init", "-b", "main"]);
        configure_git(remote);

        // 5 commits on main — the merge-base will be commit 0.
        for i in 0..5 {
            commit_file(remote, "f.txt", &format!("v{i}\n"), &format!("commit {i}"));
        }

        // Branch off from commit 0 (initial), add a PR commit.
        let fork_sha = {
            let out = Command::new("git")
                .args(["rev-list", "--max-parents=0", "HEAD"])
                .current_dir(remote)
                .output()
                .expect("rev-list");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(remote, &["checkout", "-b", "pr-branch", &fork_sha]);
        commit_file(remote, "pr.txt", "pr\n", "PR commit");

        // Create a shallow clone of the PR branch using init+remote+fetch
        // (git clone --depth=1 ignores --depth for local file transports).
        let clone_dir = tempdir().expect("tempdir clone");
        let clone = clone_dir.path();
        git(clone, &["init", "-b", "pr-branch"]);
        configure_git(clone);
        git(clone, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(clone, &["fetch", "--depth=1", "origin", "pr-branch"]);
        git(clone, &["checkout", "-b", "pr-branch", "FETCH_HEAD"]);
        // Fetch origin/main at depth 1 so its ref exists locally.
        git(clone, &["fetch", "--depth=1", "origin", "main"]);

        assert!(
            is_shallow(clone_dir.path()).expect("is_shallow"),
            "clone must start shallow"
        );

        // The merge-base (commit 0) is 1 commit behind pr-branch and 4 behind
        // main; a --deepen=50 will bring it in.
        ensure_history(
            clone_dir.path(),
            VcsKind::Git,
            "origin/main",
            &Scenario::PullRequest {
                base_branch: "origin/main".to_owned(),
            },
        )
        .expect("ensure_history PR");

        // After ensure_history, merge-base must succeed.
        assert!(
            is_merge_base_reachable(clone_dir.path(), "origin/main").expect("reachability"),
            "merge-base must be reachable after deepening"
        );

        drop(remote_dir);
    }

    // ── ensure_history: unreachable after all attempts → clear error ──────────

    #[test]
    fn ensure_history_errors_when_base_permanently_unreachable() {
        // A shallow clone where the remote doesn't have the requested ref at all.
        let (remote_dir, clone) = make_shallow_clone(3);
        let clone_path = clone.path();

        // "origin/totally-nonexistent-branch" is not present on the remote,
        // so after all deepen attempts + unshallow, merge-base still fails.
        let result = ensure_history(
            clone_path,
            VcsKind::Git,
            "origin/totally-nonexistent-branch",
            &Scenario::PullRequest {
                base_branch: "origin/totally-nonexistent-branch".to_owned(),
            },
        );

        let err = result.expect_err("expected an error for permanently unreachable base");
        let msg = err.to_string();
        assert!(
            msg.contains("origin/totally-nonexistent-branch"),
            "error must name the ref: {msg}"
        );
        assert!(
            msg.contains("git fetch origin"),
            "error must include the remedy: {msg}"
        );
        assert!(
            msg.contains("--unshallow"),
            "error must list --unshallow in what was tried: {msg}"
        );

        drop(remote_dir);
    }

    // ── resolve_git_root ──────────────────────────────────────────────────────

    #[test]
    fn resolve_git_root_returns_root_for_git_vcs() {
        let dir = tempdir().expect("tempdir");
        let result = resolve_git_root(dir.path(), VcsKind::Git).expect("resolve");
        assert_eq!(result, dir.path());
    }

    #[test]
    fn resolve_git_root_errors_for_jj_without_colocated_git() {
        let dir = tempdir().expect("tempdir");
        // No .git directory → should fail.
        let result = resolve_git_root(dir.path(), VcsKind::Jujutsu);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(".git"), "error should mention .git: {msg}");
    }

    #[test]
    fn resolve_git_root_accepts_jj_with_colocated_git_dir() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let result = resolve_git_root(dir.path(), VcsKind::Jujutsu).expect("resolve");
        assert_eq!(result, dir.path());
    }
}
