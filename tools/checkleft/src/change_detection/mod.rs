pub mod base;
pub mod default_branch;
pub mod environment;
pub mod scenario;
pub mod shallow;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use tracing::info;

use crate::vcs::{Vcs, VcsKind};

use self::base::{BaseSelection, EmptyReason, GitHeadProber, HeadProber, select_base};
use self::default_branch::{RefProber, resolve_default_branch};
use self::environment::CiEnvironment;
use self::scenario::{Scenario, classify};
use self::shallow::ensure_history;

/// Explicit overrides from CLI flags; applied before scenario classification.
pub struct ChangeOverrides {
    /// `--all`: ignore scope, check every tracked file.
    pub all: bool,
    /// `--base-ref`: bypass classification, use explicit `merge-base(ref, HEAD)`.
    pub base_ref: Option<String>,
    /// `--default-branch`: override the detected default/integration branch.
    pub default_branch: Option<String>,
}

/// The resolved change plan returned by [`resolve_change_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangePlan {
    /// Check every tracked file (`--all`).
    All,
    /// Check only changes from `base_sha` to HEAD for the classified `scenario`.
    Scoped { base_sha: String, scenario: Scenario },
    /// Nothing to check — first commit, no merge-base, or detached HEAD with no parent.
    Empty { reason: EmptyReason },
}

/// Production [`RefProber`] that shells out to git.
pub(crate) struct GitRefProber<'a> {
    root: &'a Path,
}

impl<'a> GitRefProber<'a> {
    pub(crate) fn new(root: &'a Path) -> Self {
        Self { root }
    }
}

impl RefProber for GitRefProber<'_> {
    fn resolve_symbolic_ref(&self, refname: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["symbolic-ref", "--quiet", refname])
            .current_dir(self.root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8(output.stdout).ok()?;
        let s = s.trim();
        if s.is_empty() { None } else { Some(s.to_owned()) }
    }

    fn ref_exists(&self, refname: &str) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", refname])
            .current_dir(self.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Compute what to check and from which base.
///
/// Override precedence (evaluated before environment classification):
/// 1. `overrides.all` → [`ChangePlan::All`] — no git calls made.
/// 2. `overrides.base_ref` (non-empty) → [`ChangePlan::Scoped`] with
///    `merge-base(ref, HEAD)` — back-compat escape hatch that preserves today's
///    exact idempotent semantics without touching classification.
/// 3. Classify env → resolve default branch → ensure history (deepen if shallow)
///    → select base per the scenario matrix → [`ChangePlan`].
pub fn resolve_change_plan(
    env: &CiEnvironment,
    vcs: &Vcs,
    overrides: &ChangeOverrides,
) -> Result<ChangePlan> {
    // ── 1. --all: skip all classification. ───────────────────────────────────
    if overrides.all {
        info!("--all override active: checking all tracked files");
        return Ok(ChangePlan::All);
    }

    // ── 2. --base-ref: bypass classification; keep merge-base semantics. ─────
    if let Some(base_ref) = overrides.base_ref.as_deref().filter(|s| !s.trim().is_empty()) {
        info!(base_ref, "--base-ref override: computing merge-base(ref, HEAD)");
        let prober = GitHeadProber::new(vcs.root());
        let base_sha = prober.merge_base(base_ref).ok_or_else(|| {
            anyhow::anyhow!(
                "git merge-base: no common ancestor between `{base_ref}` and HEAD"
            )
        })?;
        info!(base_ref, base_sha, "--base-ref resolved to sha");
        return Ok(ChangePlan::Scoped {
            base_sha,
            scenario: Scenario::Local,
        });
    }

    // ── 3. Classify, resolve default branch, ensure history, select base. ────
    let root = vcs.root();
    let kind = vcs.kind();
    let ref_prober = GitRefProber::new(root);
    let default_branch =
        resolve_default_branch(env, &ref_prober, overrides.default_branch.as_deref());
    info!(default_branch, "resolved default branch");

    let scenario = classify(env, &default_branch);
    info!(?scenario, "classified CI scenario");

    // The ref whose reachability we need to ensure before computing the base.
    // For PullRequest and PushToBranch we use origin/<branch> so that
    // shallow-clone deepening fetches from the remote and select_base gets a
    // fresh ref (matching what the legacy checks.sh did with
    // `git fetch origin main`).
    let needed_ref: String = match &scenario {
        Scenario::PullRequest { base_branch } => format!("origin/{base_branch}"),
        Scenario::PushToBranch { .. } => format!("origin/{default_branch}"),
        Scenario::Local => default_branch.clone(),
        Scenario::MergeQueue | Scenario::PushToDefault => "HEAD^1".to_owned(),
    };

    ensure_history(root, kind, &needed_ref, &scenario)?;

    let head_prober = GitHeadProber::new(root);
    let base_selection = select_base(&scenario, env, &head_prober, &default_branch);
    info!(?base_selection, "selected base revision");

    Ok(match base_selection {
        BaseSelection::Scoped { base_sha } | BaseSelection::WorkingTree { base_sha } => {
            info!(base_sha, "base sha resolved");
            ChangePlan::Scoped { base_sha, scenario }
        }
        BaseSelection::Empty(reason) => {
            info!(?reason, "no base available; change set will be empty");
            ChangePlan::Empty { reason }
        }
    })
}

/// Derive the base revision for source-tree reads from a resolved change plan.
///
/// Uses the same `base_sha` as the diff so the changed-file set and base-tree
/// reads are always consistent (no independent re-derivation).
pub fn base_revision_from_plan(
    vcs: &Vcs,
    plan: &ChangePlan,
) -> Option<crate::vcs::BaseRevision> {
    match plan {
        ChangePlan::All | ChangePlan::Empty { .. } => None,
        ChangePlan::Scoped { base_sha, .. } => Some(match vcs.kind() {
            VcsKind::Jujutsu => crate::vcs::BaseRevision::Jujutsu(base_sha.clone()),
            VcsKind::Git => crate::vcs::BaseRevision::Git(base_sha.clone()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;
    use crate::change_detection::environment::CiEnvironment;
    use crate::vcs::BaseRevision;

    fn git(root: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn make_repo_one_commit() -> (tempfile::TempDir, Vcs) {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a.txt"), "a").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-m", "initial"]);
        let vcs = Vcs::detect(root).unwrap();
        (tmp, vcs)
    }

    fn make_repo_two_commits() -> (tempfile::TempDir, Vcs) {
        let (tmp, _) = make_repo_one_commit();
        let root = tmp.path().to_owned();
        fs::write(root.join("b.txt"), "b").unwrap();
        git(&root, &["add", "b.txt"]);
        git(&root, &["commit", "-m", "second"]);
        let vcs = Vcs::detect(&root).unwrap();
        (tmp, vcs)
    }

    // ── Override precedence ───────────────────────────────────────────────────

    #[test]
    fn override_all_returns_plan_all() {
        let (_tmp, vcs) = make_repo_one_commit();
        let plan = resolve_change_plan(
            &CiEnvironment::default(),
            &vcs,
            &ChangeOverrides { all: true, base_ref: None, default_branch: None },
        )
        .unwrap();
        assert_eq!(plan, ChangePlan::All);
    }

    #[test]
    fn override_all_beats_base_ref() {
        let (_tmp, vcs) = make_repo_one_commit();
        let plan = resolve_change_plan(
            &CiEnvironment::default(),
            &vcs,
            &ChangeOverrides {
                all: true,
                base_ref: Some("main".to_owned()),
                default_branch: None,
            },
        )
        .unwrap();
        assert_eq!(plan, ChangePlan::All);
    }

    #[test]
    fn override_base_ref_returns_scoped_with_merge_base() {
        let (_tmp, vcs) = make_repo_two_commits();
        let plan = resolve_change_plan(
            &CiEnvironment::default(),
            &vcs,
            &ChangeOverrides {
                all: false,
                base_ref: Some("HEAD^1".to_owned()),
                default_branch: None,
            },
        )
        .unwrap();
        assert!(
            matches!(plan, ChangePlan::Scoped { .. }),
            "expected Scoped, got {plan:?}"
        );
        if let ChangePlan::Scoped { scenario, .. } = &plan {
            assert_eq!(*scenario, Scenario::Local, "back-compat base-ref uses Local scenario");
        }
    }

    #[test]
    fn empty_base_ref_string_bypasses_base_ref_override() {
        let (_tmp, vcs) = make_repo_one_commit();
        let plan = resolve_change_plan(
            &CiEnvironment::default(),
            &vcs,
            &ChangeOverrides {
                all: false,
                base_ref: Some("  ".to_owned()),
                default_branch: None,
            },
        )
        .unwrap();
        // Falls through to classification; must not be All.
        assert_ne!(plan, ChangePlan::All);
    }

    // ── base_revision_from_plan translation ───────────────────────────────────

    #[test]
    fn base_revision_from_plan_all_is_none() {
        let (_tmp, vcs) = make_repo_one_commit();
        assert_eq!(base_revision_from_plan(&vcs, &ChangePlan::All), None);
    }

    #[test]
    fn base_revision_from_plan_empty_is_none() {
        let (_tmp, vcs) = make_repo_one_commit();
        let plan = ChangePlan::Empty {
            reason: base::EmptyReason::NoMergeBase,
        };
        assert_eq!(base_revision_from_plan(&vcs, &plan), None);
    }

    #[test]
    fn base_revision_from_plan_scoped_returns_git_sha() {
        let (_tmp, vcs) = make_repo_one_commit();
        let sha = "abc123".to_owned();
        let plan = ChangePlan::Scoped {
            base_sha: sha.clone(),
            scenario: Scenario::Local,
        };
        assert_eq!(
            base_revision_from_plan(&vcs, &plan),
            Some(BaseRevision::Git(sha))
        );
    }
}
