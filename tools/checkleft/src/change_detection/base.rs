use std::path::Path;
use std::process::Command;

use super::environment::CiEnvironment;
use super::scenario::Scenario;

/// The resolved diff base for a scenario. All variants are expressed in git
/// terms (concrete SHAs or HEAD-relative references); jj translation is handled
/// at the diff layer in a later task — a git SHA is a valid jj revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseSelection {
    /// 2-dot diff: `base_sha..HEAD` (committed changes only).
    ///
    /// The SHA was resolved at selection time by `select_base` — either a
    /// pre-computed merge-base, `merge_group.base_sha`, or a resolved `HEAD^1`.
    Scoped { base_sha: String },

    /// Local / pre-push: diff from `base_sha` to the working tree, including
    /// uncommitted + staged changes. `base_sha` is the pre-resolved merge-base.
    WorkingTree { base_sha: String },

    /// Nothing to diff.
    Empty(EmptyReason),
}

/// Why there is nothing to diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmptyReason {
    /// Root commit, unrelated histories, or any case where `git merge-base`
    /// returns no common ancestor.
    NoMergeBase,
    /// Detached HEAD with no accessible parent commit.
    DetachedHeadNoParent,
}

/// Git-level probes used by [`select_base`]. Injectable for unit testing.
pub(crate) trait HeadProber {
    /// `git rev-parse <rev>` — returns the SHA if the revision exists, `None` if
    /// the revision does not exist or git fails.
    fn resolve(&self, rev: &str) -> Option<String>;

    /// `git merge-base <base_ref> HEAD` — returns the merge-base SHA, or `None`
    /// if there is no common ancestor (root commit, unrelated histories).
    fn merge_base(&self, base_ref: &str) -> Option<String>;
}

/// Production [`HeadProber`] that shells out to `git`.
pub(crate) struct GitHeadProber<'a> {
    root: &'a Path,
}

impl<'a> GitHeadProber<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }
}

impl HeadProber for GitHeadProber<'_> {
    fn resolve(&self, rev: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(self.root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sha = String::from_utf8(output.stdout).ok()?;
        let sha = sha.trim();
        if sha.is_empty() { None } else { Some(sha.to_owned()) }
    }

    fn merge_base(&self, base_ref: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["merge-base", base_ref, "HEAD"])
            .current_dir(self.root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sha = String::from_utf8(output.stdout).ok()?;
        let sha = sha.trim();
        if sha.is_empty() { None } else { Some(sha.to_owned()) }
    }
}

/// Select the base revision for a given scenario.
///
/// This is the **single centralised place** where the scenario→base matrix
/// lives. The critical asymmetry between rows 1 and 2/3 — PR uses merge-base
/// (3-dot) while merge queue uses `HEAD^1` directly (2-dot) — is encoded here,
/// side-by-side, so the contrast is impossible to miss.
///
/// Matrix summary:
///
/// | Row | Scenario           | Base                              |
/// |-----|--------------------|-----------------------------------|
/// |   1 | Regular PR         | merge-base(base_branch, HEAD)     |
/// |   2 | GitHub merge queue | merge_group.base_sha or HEAD^1    |
/// |   3 | Buildkite MQ       | HEAD^1                            |
/// |   4 | Push to default    | HEAD^1                            |
/// |   5 | Push to branch     | merge-base(default_branch, HEAD)  |
/// |   6 | Local / pre-push   | merge-base(default) + working tree|
/// |   7 | No merge-base      | Empty { NoMergeBase }             |
/// |   8 | Detached HEAD      | HEAD^1 if exists, else Empty      |
pub(crate) fn select_base(
    scenario: &Scenario,
    env: &CiEnvironment,
    prober: &dyn HeadProber,
    default_branch: &str,
) -> BaseSelection {
    match scenario {
        // ── Row 1: Regular PR ────────────────────────────────────────────────
        // 3-dot equivalent: diff against merge-base(base_branch, HEAD).
        // MUST NOT 2-dot against origin/<base_branch> directly — that sweeps in
        // changes that landed on the base branch after this PR forked (T843/#948).
        //
        // Use origin/<base_branch> when available so CI agents that reuse
        // checkout directories (Buildkite) don't see a stale local branch.
        // The legacy checks.sh always did `git fetch origin main && merge-base
        // origin/main HEAD`; we mirror that here.
        //
        // ── Rows 2 & 3: Merge queue — THE OPPOSITE RULE ─────────────────────
        // Merge queue uses HEAD^1 directly (2-dot). Do NOT compute merge-base
        // here. Using merge-base(HEAD^1, HEAD^2) returns the fork point where
        // the PR diverged from main, sweeping in all of main's drift (T774/#910).
        // The two cases are adjacent so the asymmetry is impossible to miss.
        Scenario::PullRequest { base_branch } => {
            let remote_ref = format!("origin/{base_branch}");
            let base_ref = if prober.resolve(&remote_ref).is_some() {
                remote_ref
            } else {
                base_branch.clone()
            };
            match prober.merge_base(&base_ref) {
                Some(sha) => BaseSelection::Scoped { base_sha: sha },
                None => BaseSelection::Empty(EmptyReason::NoMergeBase),
            }
        }

        Scenario::MergeQueue => select_base_merge_queue(env, prober),

        // ── Row 4: Push to default branch ────────────────────────────────────
        Scenario::PushToDefault => select_base_push_to_default(prober),

        // ── Row 5: Push to non-default branch ────────────────────────────────
        // Treated as a pre-merge branch: 3-dot against the default branch,
        // same base rule as PR but without an explicit PR signal.
        //
        // Use origin/<default_branch> when available so CI agents that reuse
        // checkout directories (Buildkite) don't see a stale local branch.
        // The legacy checks.sh always did `git fetch origin main && merge-base
        // origin/main HEAD`; we mirror that here.
        Scenario::PushToBranch { .. } => {
            let remote_ref = format!("origin/{default_branch}");
            let base_ref = if prober.resolve(&remote_ref).is_some() {
                remote_ref
            } else {
                default_branch.to_owned()
            };
            match prober.merge_base(&base_ref) {
                Some(sha) => BaseSelection::Scoped { base_sha: sha },
                None => BaseSelection::Empty(EmptyReason::NoMergeBase),
            }
        }

        // ── Rows 6 & 8: Local / pre-push with detached-HEAD fallback ─────────
        Scenario::Local => select_base_local(prober, default_branch),
    }
}

/// Rows 2 & 3: GitHub merge queue and Buildkite merge queue.
fn select_base_merge_queue(env: &CiEnvironment, prober: &dyn HeadProber) -> BaseSelection {
    // Row 2: GitHub merge queue.
    // Prefer merge_group.base_sha from the event payload (the authoritative
    // current tip of the target branch). Fall back to HEAD^1 when the payload
    // is absent — they are equivalent on a well-formed merge commit.
    if env.github_event_name.as_deref() == Some("merge_group") {
        let sha = env
            .read_github_event_payload()
            .and_then(|p| p.merge_group)
            .and_then(|mg| mg.base_sha);
        return match sha {
            Some(sha) => BaseSelection::Scoped { base_sha: sha },
            None => head_parent_or_empty(prober),
        };
    }

    // Row 3: Buildkite merge queue (BUILDKITE_BRANCH = gh-readonly-queue/<target>/…).
    // Use HEAD^1 unconditionally, exactly mirroring legacy checks.sh:
    //   git rev-parse HEAD^1
    // Do NOT check HEAD^2 to gate this: in a shallow Buildkite checkout the second
    // parent is often not fetched even when HEAD is a genuine merge commit, which
    // caused the HEAD^2 sentinel to fail and fall through to merge-base(origin/main),
    // producing 321 changed files instead of the correct 6 (T1016/#1104).
    head_parent_or_empty(prober)
}

/// Row 4: Push to the default branch.
///
/// Scope to this push only: `HEAD^1` is the commit just before the push.
///
/// Future enhancement: use the CI-provided `before` SHA when present and
/// reachable (GHA event payload `before`, `BUILDKITE_PREVIOUS_COMMIT`) to
/// correctly scope force-pushes.
fn select_base_push_to_default(prober: &dyn HeadProber) -> BaseSelection {
    head_parent_or_empty(prober)
}

/// Rows 6 & 8: Local / pre-push, with detached-HEAD fallback.
///
/// Row 6 (normal): `merge-base(default_branch, HEAD)` — includes uncommitted
/// and staged changes in the working tree.
///
/// Row 8 (fallback): if no merge-base exists (detached HEAD, unrelated histories),
/// try `HEAD^1` as a best-effort base. If HEAD has no parent either (root commit
/// on a detached HEAD), return `Empty { DetachedHeadNoParent }`.
fn select_base_local(prober: &dyn HeadProber, default_branch: &str) -> BaseSelection {
    match prober.merge_base(default_branch) {
        Some(sha) => BaseSelection::WorkingTree { base_sha: sha },
        None => {
            // Row 8: no common ancestor with the default branch.
            // Best-effort: try HEAD^1 (covers detached-HEAD CI checkouts).
            match prober.resolve("HEAD^1") {
                Some(sha) => BaseSelection::WorkingTree { base_sha: sha },
                None => BaseSelection::Empty(EmptyReason::DetachedHeadNoParent),
            }
        }
    }
}

/// Resolve `HEAD^1` and return `Scoped { base_sha }`, or `Empty` if HEAD has no
/// parent (root commit or truly orphaned HEAD).
fn head_parent_or_empty(prober: &dyn HeadProber) -> BaseSelection {
    match prober.resolve("HEAD^1") {
        Some(sha) => BaseSelection::Scoped { base_sha: sha },
        None => BaseSelection::Empty(EmptyReason::DetachedHeadNoParent),
    }
}

/// Extract the target branch from a Buildkite gh-readonly-queue branch name.
///
/// `gh-readonly-queue/main/pr-42-sha-abc` → `Some("main")`
fn parse_bk_queue_target(branch: &str) -> Option<String> {
    branch
        .strip_prefix("gh-readonly-queue/")?
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    // ── Stub prober ───────────────────────────────────────────────────────────

    #[derive(Default)]
    struct Stub {
        revs: HashMap<String, String>,
        bases: HashMap<String, String>,
    }

    impl Stub {
        fn rev(mut self, rev: &str, sha: &str) -> Self {
            self.revs.insert(rev.to_owned(), sha.to_owned());
            self
        }

        fn base(mut self, base_ref: &str, sha: &str) -> Self {
            self.bases.insert(base_ref.to_owned(), sha.to_owned());
            self
        }
    }

    impl HeadProber for Stub {
        fn resolve(&self, rev: &str) -> Option<String> {
            self.revs.get(rev).cloned()
        }
        fn merge_base(&self, base_ref: &str) -> Option<String> {
            self.bases.get(base_ref).cloned()
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    const DEFAULT: &str = "main";
    const MERGE_BASE_SHA: &str = "aabbcc001122";
    const HEAD1_SHA: &str = "deadbeefcafe";
    const MQ_BASE_SHA: &str = "112233440000";

    fn gha_env(event: &str) -> CiEnvironment {
        CiEnvironment {
            github_actions: true,
            github_event_name: Some(event.to_owned()),
            ..Default::default()
        }
    }

    fn bk_env(branch: &str) -> CiEnvironment {
        CiEnvironment {
            buildkite: true,
            buildkite_pull_request: Some("false".to_owned()),
            buildkite_branch: Some(branch.to_owned()),
            ..Default::default()
        }
    }

    fn env_with_merge_group_payload(base_sha: &str) -> CiEnvironment {
        let json = format!(
            r#"{{"merge_group":{{"base_sha":"{base_sha}","head_sha":"ignored","base_ref":"refs/heads/main"}}}}"#
        );
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let path = f.path().to_owned();
        std::mem::forget(f);
        CiEnvironment {
            github_actions: true,
            github_event_name: Some("merge_group".to_owned()),
            github_event_path: Some(path),
            ..Default::default()
        }
    }

    // ── Row 1: Regular PR ─────────────────────────────────────────────────────

    #[test]
    fn row1_pr_uses_merge_base_of_base_branch() {
        let scenario = Scenario::PullRequest {
            base_branch: DEFAULT.to_owned(),
        };
        let env = gha_env("pull_request");
        let prober = Stub::default().base(DEFAULT, MERGE_BASE_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row1_pr_uses_explicit_base_branch_not_default() {
        let scenario = Scenario::PullRequest {
            base_branch: "release/1.x".to_owned(),
        };
        let env = gha_env("pull_request");
        let prober = Stub::default().base("release/1.x", MERGE_BASE_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row1_pr_prefers_origin_ref_over_stale_local() {
        let scenario = Scenario::PullRequest {
            base_branch: DEFAULT.to_owned(),
        };
        let env = gha_env("pull_request");
        // origin/main is present and fresher; local "main" is stale.
        // select_base must use origin/main, not local main.
        let prober = Stub::default()
            .rev("origin/main", "some_origin_sha") // origin/main resolves
            .base("origin/main", MERGE_BASE_SHA)
            .base(DEFAULT, "stale_local_sha");
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    // ── Row 2: GitHub merge queue ─────────────────────────────────────────────

    #[test]
    fn row2_gha_merge_queue_uses_base_sha_from_payload() {
        let scenario = Scenario::MergeQueue;
        let env = env_with_merge_group_payload(MQ_BASE_SHA);
        let prober = Stub::default(); // payload-based, no prober calls needed
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MQ_BASE_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row2_gha_merge_queue_falls_back_to_head_parent_when_no_payload() {
        let scenario = Scenario::MergeQueue;
        let env = gha_env("merge_group"); // no GITHUB_EVENT_PATH
        let prober = Stub::default().rev("HEAD^1", HEAD1_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: HEAD1_SHA.to_owned()
            }
        );
    }

    // ── Row 3: Buildkite merge queue ──────────────────────────────────────────

    #[test]
    fn row3_bk_merge_queue_uses_head_parent() {
        let scenario = Scenario::MergeQueue;
        let env = bk_env("gh-readonly-queue/main/pr-42-sha-abc");
        // HEAD^1 is the first parent of the merge commit (matches legacy checks.sh).
        // HEAD^2 is not consulted — it may be absent in a shallow Buildkite checkout.
        let prober = Stub::default().rev("HEAD^1", HEAD1_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: HEAD1_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row3_bk_merge_queue_head_parent_absent_gives_empty() {
        let scenario = Scenario::MergeQueue;
        let env = bk_env("gh-readonly-queue/main/pr-42-sha-abc");
        // Neither HEAD^1 nor HEAD^2 resolves → Empty (no fallback to merge-base).
        let prober = Stub::default();
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Empty(EmptyReason::DetachedHeadNoParent)
        );
    }

    #[test]
    fn row3_bk_merge_queue_head_parent_used_regardless_of_head2_presence() {
        // HEAD^2 present (genuine merge commit) and HEAD^1 also present.
        // Result is still HEAD^1 — HEAD^2 is no longer a gating condition.
        let scenario = Scenario::MergeQueue;
        let env = bk_env("gh-readonly-queue/main/pr-42-sha-abc");
        let prober = Stub::default()
            .rev("HEAD^2", "secondparentsha")
            .rev("HEAD^1", HEAD1_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: HEAD1_SHA.to_owned()
            }
        );
    }

    // ── Row 4: Push to default ────────────────────────────────────────────────

    #[test]
    fn row4_push_to_default_uses_head_parent() {
        let scenario = Scenario::PushToDefault;
        let env = gha_env("push");
        let prober = Stub::default().rev("HEAD^1", HEAD1_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: HEAD1_SHA.to_owned()
            }
        );
    }

    // ── Row 5: Push to non-default branch ────────────────────────────────────

    #[test]
    fn row5_push_to_branch_uses_merge_base_of_default() {
        let scenario = Scenario::PushToBranch {
            branch: "feature/foo".to_owned(),
        };
        let env = gha_env("push");
        // origin/main not present → falls back to local "main"
        let prober = Stub::default().base(DEFAULT, MERGE_BASE_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row5_push_to_branch_prefers_origin_ref_over_stale_local() {
        let scenario = Scenario::PushToBranch {
            branch: "feature/foo".to_owned(),
        };
        let env = gha_env("push");
        // origin/main is present and fresher; local "main" is stale.
        // select_base must use origin/main, not local main.
        let prober = Stub::default()
            .rev("origin/main", "some_origin_sha") // origin/main resolves
            .base("origin/main", MERGE_BASE_SHA)
            .base(DEFAULT, "stale_local_sha");
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Scoped {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    // ── Row 6: Local ──────────────────────────────────────────────────────────

    #[test]
    fn row6_local_uses_working_tree_on_merge_base() {
        let scenario = Scenario::Local;
        let env = CiEnvironment::default();
        let prober = Stub::default().base(DEFAULT, MERGE_BASE_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::WorkingTree {
                base_sha: MERGE_BASE_SHA.to_owned()
            }
        );
    }

    // ── Row 7: No merge-base ──────────────────────────────────────────────────

    #[test]
    fn row7_pr_no_merge_base_returns_empty_no_merge_base() {
        let scenario = Scenario::PullRequest {
            base_branch: DEFAULT.to_owned(),
        };
        let env = gha_env("pull_request");
        let prober = Stub::default(); // no merge-base configured
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Empty(EmptyReason::NoMergeBase)
        );
    }

    #[test]
    fn row7_push_to_branch_no_merge_base_returns_empty() {
        let scenario = Scenario::PushToBranch {
            branch: "orphan-branch".to_owned(),
        };
        let env = gha_env("push");
        let prober = Stub::default(); // no merge-base
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Empty(EmptyReason::NoMergeBase)
        );
    }

    // ── Row 8: Detached HEAD ──────────────────────────────────────────────────

    #[test]
    fn row8_local_no_merge_base_falls_back_to_head_parent() {
        let scenario = Scenario::Local;
        let env = CiEnvironment::default();
        // No merge-base (detached HEAD / unrelated histories), but HEAD^1 exists
        let prober = Stub::default().rev("HEAD^1", HEAD1_SHA);
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::WorkingTree {
                base_sha: HEAD1_SHA.to_owned()
            }
        );
    }

    #[test]
    fn row8_local_no_merge_base_no_parent_returns_empty() {
        let scenario = Scenario::Local;
        let env = CiEnvironment::default();
        // Neither merge-base nor HEAD^1 → root commit on detached HEAD
        let prober = Stub::default();
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Empty(EmptyReason::DetachedHeadNoParent)
        );
    }

    #[test]
    fn row8_push_to_default_no_parent_returns_empty() {
        let scenario = Scenario::PushToDefault;
        let env = gha_env("push");
        let prober = Stub::default(); // no HEAD^1
        assert_eq!(
            select_base(&scenario, &env, &prober, DEFAULT),
            BaseSelection::Empty(EmptyReason::DetachedHeadNoParent)
        );
    }

    // ── parse_bk_queue_target ─────────────────────────────────────────────────

    #[test]
    fn parse_bk_queue_target_extracts_target_branch() {
        assert_eq!(
            parse_bk_queue_target("gh-readonly-queue/main/pr-42-abc"),
            Some("main".to_owned())
        );
        assert_eq!(
            parse_bk_queue_target("gh-readonly-queue/master/pr-7"),
            Some("master".to_owned())
        );
        assert_eq!(
            parse_bk_queue_target("gh-readonly-queue/develop/pr-1-sha"),
            Some("develop".to_owned())
        );
    }

    #[test]
    fn parse_bk_queue_target_returns_none_for_non_queue_branch() {
        assert_eq!(parse_bk_queue_target("main"), None);
        assert_eq!(parse_bk_queue_target("feature/my-pr"), None);
        assert_eq!(parse_bk_queue_target("gh-readonly-queue/"), None);
    }
}
