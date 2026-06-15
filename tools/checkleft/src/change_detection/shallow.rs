use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use tracing::info;

use crate::vcs::VcsKind;

use super::scenario::Scenario;

/// Successive `--deepen=<N>` increments tried before falling back to full
/// `--unshallow`. Each step adds N commits to the existing shallow depth.
pub const DEEPEN_LADDER: &[u32] = &[50, 250, 1000];

/// Ensure the local git repo has enough history to compute a base for
/// `needed_ref`.
///
/// # Return value
///
/// Returns `Ok(true)` when the needed history is available, `Ok(false)` when
/// the ref is permanently unreachable after all deepening/unshallowing attempts
/// (caller decides how to handle this), or `Err` for genuine git failures
/// (network errors, git not found, jj import failure, etc.).
///
/// # Behaviour
///
/// - Not shallow → returns `Ok(true)` immediately.
/// - Merge-queue/push-to-default scenarios only need `HEAD^1`; a single
///   `--deepen=1` is sufficient and always succeeds (returning `Ok(true)`).
/// - PR/branch/local scenarios need the merge-base against `needed_ref`;
///   the bounded ladder `--deepen=50 → 250 → 1000` is tried with a
///   reachability check after each step, then `--unshallow` as last resort.
///   Returns `Ok(false)` if the ref is still unreachable after all attempts.
///
/// # jj colocated repos
///
/// jj's colocated git repo is what is shallow. We operate on the underlying
/// git repo via the `.git` directory at the workspace root and then run
/// `jj git import` to sync jj's op log after deepening.
pub fn ensure_history(root: &Path, kind: VcsKind, needed_ref: &str, scenario: &Scenario) -> Result<bool> {
    // Secondary jj workspaces have no colocated .git at their root. These are
    // local development environments, not CI shallow clones, so there is no
    // shallow history to deepen. Skip the check and report history as available.
    // Invariant: secondary jj workspaces always share a non-shallow primary
    // store (cube workspaces are full clones), so skipping the depth check
    // here is safe — if this assumption ever breaks (shallow primary store),
    // this early return must be removed.
    if kind == VcsKind::Jujutsu && !root.join(".git").exists() {
        info!("secondary jj workspace (no colocated .git) — skipping shallow history check");
        return Ok(true);
    }

    let git_root = resolve_git_root(root, kind)?;

    // For PR and push-to-branch scenarios, always refresh the remote tracking
    // ref before checking depth. Buildkite agents reuse checkout directories
    // and only fetch the specific PR commit SHA — origin/<base_branch> can be
    // stale, causing merge-base to be computed against an old tip and pulling
    // unrelated main-branch changes into the "PR diff". Best-effort: ignore
    // network failures, missing credentials, etc.
    match scenario {
        Scenario::PullRequest { .. } | Scenario::PushToBranch { .. } => {
            if let Some(branch) = needed_ref.strip_prefix("origin/") {
                let _ = Command::new("git")
                    .args(["fetch", "origin", branch, "--no-tags"])
                    .current_dir(&git_root)
                    .output();
                info!(branch, "refreshed remote tracking ref before merge-base");
            }
        }
        _ => {}
    }

    if !is_shallow(&git_root)? {
        return Ok(true);
    }

    info!(needed_ref, "repo is shallow; deepening history");

    let reached = match scenario {
        // ── Merge-queue / push-to-default: only HEAD^1 is needed ─────────────
        // A single --deepen=1 brings the parent commit into local history.
        // No merge-base computation is required for these scenarios.
        Scenario::MergeQueue | Scenario::PushToDefault => {
            deepen(&git_root, 1)?;
            info!("deepened by 1 to reach HEAD^1 for merge-queue/push-to-default scenario");
            true
        }

        // ── PR / push-to-branch / local: need the merge-base against needed_ref
        // Try the bounded ladder then full unshallow before reporting unreachable.
        // PushToBranch needs merge-base(origin/<default_branch>, HEAD), so it
        // requires the same treatment as PR and Local — not just HEAD^1.
        Scenario::PullRequest { .. } | Scenario::PushToBranch { .. } | Scenario::Local => {
            deepen_until_reachable(&git_root, needed_ref)?
        }
    };

    if reached {
        // In jj colocated repos: sync jj's op log so jj commands see the deepened
        // history. See design Q3 — jj git import is needed after git-level fetches.
        if kind == VcsKind::Jujutsu {
            import_jj_git(&git_root)?;
        }
    }

    Ok(reached)
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
    let output = Command::new("jj").args(["git", "import"]).current_dir(root).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`jj git import` failed after history deepening: {}", stderr.trim());
    }
    info!("ran `jj git import` to sync after history deepening");
    Ok(())
}

/// Fetch a specific branch from origin into its tracking ref, bypassing any
/// single-branch fetch refspec restriction in the clone config.
///
/// In single-branch Buildkite shallow clones the configured refspec only covers
/// the pushed branch (e.g. `+refs/heads/boss/…:refs/remotes/origin/boss/…`).
/// A plain `git fetch --deepen origin` therefore never creates `origin/main`.
/// Passing an explicit refspec on the command line overrides the config for that
/// invocation, creating `refs/remotes/origin/<branch>` unconditionally.
///
/// Returns `Ok(true)` when the fetch succeeded, `Ok(false)` when the branch
/// does not exist on the remote (so the caller can propagate "unreachable").
fn fetch_origin_branch(root: &Path, branch: &str) -> Result<bool> {
    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    let output = Command::new("git")
        .args(["fetch", "--no-tags", "--depth=1", "origin", &refspec])
        .current_dir(root)
        .output()?;
    Ok(output.status.success())
}

/// Deepen a specific remote branch by N commits using an explicit refspec.
///
/// Like `fetch_origin_branch`, using an explicit refspec ensures deepening
/// works even in single-branch clones whose fetch config would otherwise
/// only cover the pushed branch.
fn deepen_branch(root: &Path, branch: &str, n: u32) -> Result<()> {
    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    let depth_arg = format!("--deepen={n}");
    run_git(root, &["fetch", "--no-tags", &depth_arg, "origin", &refspec])
}

/// Try the deepen ladder then full unshallow until `git merge-base <base_ref>
/// HEAD` succeeds. Returns `true` if reachable, `false` if not (even after
/// unshallow).
///
/// For `origin/<branch>` refs, an explicit targeted fetch is attempted first
/// to ensure the ref exists locally — necessary in single-branch shallow clones
/// where `git fetch --deepen origin` would only deepen the currently-checked-out
/// branch and never create `origin/main`.
fn deepen_until_reachable(root: &Path, base_ref: &str) -> Result<bool> {
    // Step 0: For origin/<branch> refs, fetch the branch explicitly.
    // In single-branch shallow clones, origin/<branch> may not exist at all.
    // An explicit refspec overrides the clone's restricted fetch config.
    let origin_branch = base_ref.strip_prefix("origin/");
    if let Some(branch) = origin_branch {
        info!(
            base_ref,
            "fetching base branch explicitly to bypass single-branch fetch config"
        );
        if !fetch_origin_branch(root, branch)? {
            // Branch doesn't exist on the remote; nothing more we can do.
            info!(base_ref, "explicit fetch failed: branch not present on remote");
            return Ok(false);
        }
        if is_merge_base_reachable(root, base_ref)? {
            info!(base_ref, "merge-base reachable after explicit fetch");
            return Ok(true);
        }
        // Branch ref now exists but merge-base is still not reachable (the fork
        // point is deeper than depth=1). Fall through to the deepen ladder.
        info!(base_ref, "branch fetched; merge-base not yet reachable — deepening");
    }

    for &step in DEEPEN_LADDER {
        if let Some(branch) = origin_branch {
            deepen_branch(root, branch, step)?;
        } else {
            deepen(root, step)?;
        }
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

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    String::from_utf8(output.stdout).map_err(|e| anyhow::anyhow!("git output was not valid utf-8: {e}"))
}

fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
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

    fn strip_origin_prefix(r: &str) -> &str {
        r.strip_prefix("origin/").unwrap_or(r)
    }

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
        assert!(is_shallow(clone_path).expect("is_shallow"), "clone must start shallow");

        // After ensure_history with MergeQueue, HEAD^1 must be reachable.
        ensure_history(clone_path, VcsKind::Git, "HEAD^1", &Scenario::MergeQueue).expect("ensure_history MergeQueue");

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

    // ── ensure_history: unreachable after all attempts → Ok(false) ─────────────

    #[test]
    fn ensure_history_returns_false_when_base_permanently_unreachable() {
        // A shallow clone where the remote doesn't have the requested ref at all.
        let (remote_dir, clone) = make_shallow_clone(3);
        let clone_path = clone.path();

        // "origin/totally-nonexistent-branch" is not present on the remote,
        // so after all deepen attempts + unshallow, merge-base still fails.
        // ensure_history now returns Ok(false) so the caller can decide how
        // to handle the unreachable case (error for PR, empty for push, etc.).
        let reached = ensure_history(
            clone_path,
            VcsKind::Git,
            "origin/totally-nonexistent-branch",
            &Scenario::PullRequest {
                base_branch: "origin/totally-nonexistent-branch".to_owned(),
            },
        )
        .expect("ensure_history must not return Err for unreachable (only Ok(false))");

        assert!(!reached, "permanently unreachable ref must yield reached=false");

        drop(remote_dir);
    }

    // ── resolve_git_root ──────────────────────────────────────────────────────

    #[test]
    fn resolve_git_root_returns_root_for_git_vcs() {
        let dir = tempdir().expect("tempdir");
        let result = resolve_git_root(dir.path(), VcsKind::Git).expect("resolve");
        assert_eq!(result, dir.path());
    }

    // ── ensure_history: secondary jj workspace skips check ───────────────────

    #[test]
    fn ensure_history_is_noop_for_secondary_jj_workspace() {
        // A directory with no .git is a secondary jj workspace.
        // ensure_history must return Ok(true) rather than bailing.
        let dir = tempdir().expect("tempdir");
        let result = ensure_history(
            dir.path(),
            VcsKind::Jujutsu,
            "origin/main",
            &Scenario::PullRequest {
                base_branch: "main".to_owned(),
            },
        );
        assert!(
            matches!(result, Ok(true)),
            "secondary jj workspace must return Ok(true), got {result:?}"
        );
    }

    // ── resolve_git_root ──────────────────────────────────────────────────────

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
