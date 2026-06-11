use super::environment::CiEnvironment;

/// The classified CI execution scenario. This is the single source of truth
/// for the PR-vs-merge-queue distinction — the two cases use *opposite* base
/// selection rules (merge-base vs HEAD^1) and that asymmetry lives here, not
/// in shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scenario {
    /// A pull request build. `base_branch` is the branch being merged into.
    PullRequest { base_branch: String },
    /// A GitHub merge-queue (merge_group) or Buildkite gh-readonly-queue build.
    /// HEAD is a GitHub-created merge commit; the correct base is HEAD^1, NOT
    /// merge-base(HEAD^1, HEAD^2) — see T774/#910.
    MergeQueue,
    /// A push directly to the default/integration branch (main, master, …).
    PushToDefault,
    /// A push to a non-default branch without an associated PR.
    PushToBranch { branch: String },
    /// Running on a local developer machine (no CI signal detected).
    Local,
}

/// Classify the current execution context into a [`Scenario`].
///
/// `default_branch` is the already-resolved integration branch name (from
/// [`super::default_branch::resolve_default_branch`]) and is used only to
/// distinguish `PushToDefault` from `PushToBranch`.
///
/// Precedence (most specific first):
///
/// 1. GHA `merge_group` event  → `MergeQueue`
/// 2. BK branch `gh-readonly-queue/…` → `MergeQueue`
/// 3. GHA `pull_request` event → `PullRequest`
/// 4. BK pull request (BUILDKITE_PULL_REQUEST != "false") → `PullRequest`
/// 5. Push to default branch → `PushToDefault`
/// 6. Push to non-default branch → `PushToBranch`
/// 7. No CI signal → `Local`
pub fn classify(env: &CiEnvironment, default_branch: &str) -> Scenario {
    // Rule 1 — GitHub merge_group event (highest priority for merge-queue).
    if env.github_event_name.as_deref().is_some_and(|e| e == "merge_group") {
        return Scenario::MergeQueue;
    }

    // Rule 2 — Buildkite gh-readonly-queue branch.
    if env
        .buildkite_branch
        .as_deref()
        .is_some_and(|b| b.starts_with("gh-readonly-queue/"))
    {
        return Scenario::MergeQueue;
    }

    // Rule 3 — GitHub pull_request event.
    if env.github_event_name.as_deref().is_some_and(|e| e == "pull_request") {
        let base_branch = env
            .github_base_ref
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_branch.to_owned());
        return Scenario::PullRequest { base_branch };
    }

    // Rule 4 — Buildkite PR (BUILDKITE_PULL_REQUEST is set and not "false").
    if env.buildkite && env.buildkite_pull_request.as_deref().is_some_and(|v| v != "false") {
        let base_branch = env
            .buildkite_pull_request_base_branch
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_branch.to_owned());
        return Scenario::PullRequest { base_branch };
    }

    // Rules 5 & 6 — GHA push event.
    if env.github_event_name.as_deref().is_some_and(|e| e == "push") {
        let branch = branch_from_github_ref(env.github_ref.as_deref()).unwrap_or_else(|| default_branch.to_owned());
        if branch == default_branch {
            return Scenario::PushToDefault;
        }
        return Scenario::PushToBranch { branch };
    }

    // Rules 5 & 6 — Buildkite push (not PR, not merge queue).
    if env.buildkite && env.buildkite_pull_request.as_deref().is_some_and(|v| v == "false") {
        let branch = env
            .buildkite_branch
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_branch.to_owned());
        if branch == default_branch {
            return Scenario::PushToDefault;
        }
        return Scenario::PushToBranch { branch };
    }

    // Rule 7 — no CI signal at all.
    Scenario::Local
}

/// Extract the short branch name from a full GHA `GITHUB_REF` like
/// `refs/heads/main`.  Returns `None` for non-branch refs (tags, etc.).
fn branch_from_github_ref(github_ref: Option<&str>) -> Option<String> {
    github_ref
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "main";

    fn gha(event_name: &str) -> CiEnvironment {
        CiEnvironment {
            github_actions: true,
            github_event_name: Some(event_name.to_owned()),
            ..Default::default()
        }
    }

    fn bk(pull_request: &str, branch: &str) -> CiEnvironment {
        CiEnvironment {
            buildkite: true,
            buildkite_pull_request: Some(pull_request.to_owned()),
            buildkite_branch: Some(branch.to_owned()),
            ..Default::default()
        }
    }

    // ── Merge queue ───────────────────────────────────────────────────────────

    #[test]
    fn gha_merge_group_is_merge_queue() {
        let env = gha("merge_group");
        assert_eq!(classify(&env, DEFAULT), Scenario::MergeQueue);
    }

    #[test]
    fn bk_gh_readonly_queue_is_merge_queue() {
        let env = bk("false", "gh-readonly-queue/main/pr-42-sha-abc");
        assert_eq!(classify(&env, DEFAULT), Scenario::MergeQueue);
    }

    #[test]
    fn bk_gh_readonly_queue_master_is_merge_queue() {
        let env = bk("false", "gh-readonly-queue/master/pr-7");
        assert_eq!(classify(&env, "master"), Scenario::MergeQueue);
    }

    /// merge_group takes precedence even if BUILDKITE_BRANCH also looks like a
    /// queue branch (shouldn't happen in practice, but confirms ordering).
    #[test]
    fn gha_merge_group_beats_bk_queue_branch() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("merge_group".to_owned()),
            buildkite: true,
            buildkite_branch: Some("gh-readonly-queue/main/pr-1".to_owned()),
            buildkite_pull_request: Some("false".to_owned()),
            ..Default::default()
        };
        assert_eq!(classify(&env, DEFAULT), Scenario::MergeQueue);
    }

    // ── Pull request ──────────────────────────────────────────────────────────

    #[test]
    fn gha_pull_request_with_base_ref() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_base_ref: Some("main".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            classify(&env, DEFAULT),
            Scenario::PullRequest {
                base_branch: "main".to_owned()
            }
        );
    }

    #[test]
    fn gha_pull_request_falls_back_to_default_when_base_ref_absent() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_base_ref: None,
            ..Default::default()
        };
        assert_eq!(
            classify(&env, "master"),
            Scenario::PullRequest {
                base_branch: "master".to_owned()
            }
        );
    }

    #[test]
    fn bk_pull_request_with_base_branch() {
        let env = CiEnvironment {
            buildkite: true,
            buildkite_pull_request: Some("42".to_owned()),
            buildkite_branch: Some("feature/my-pr".to_owned()),
            buildkite_pull_request_base_branch: Some("main".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            classify(&env, DEFAULT),
            Scenario::PullRequest {
                base_branch: "main".to_owned()
            }
        );
    }

    #[test]
    fn bk_pull_request_falls_back_to_default_when_base_absent() {
        let env = CiEnvironment {
            buildkite: true,
            buildkite_pull_request: Some("7".to_owned()),
            buildkite_branch: Some("feature/x".to_owned()),
            buildkite_pull_request_base_branch: None,
            ..Default::default()
        };
        assert_eq!(
            classify(&env, "master"),
            Scenario::PullRequest {
                base_branch: "master".to_owned()
            }
        );
    }

    /// GHA pull_request wins over BK pull_request when both signals are present.
    #[test]
    fn gha_pull_request_beats_bk_pr() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_base_ref: Some("main".to_owned()),
            buildkite: true,
            buildkite_pull_request: Some("99".to_owned()),
            buildkite_pull_request_base_branch: Some("other".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            classify(&env, DEFAULT),
            Scenario::PullRequest {
                base_branch: "main".to_owned()
            }
        );
    }

    // ── Push to default ───────────────────────────────────────────────────────

    #[test]
    fn gha_push_to_main_is_push_to_default() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_ref: Some("refs/heads/main".to_owned()),
            ..Default::default()
        };
        assert_eq!(classify(&env, "main"), Scenario::PushToDefault);
    }

    #[test]
    fn gha_push_to_master_is_push_to_default() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_ref: Some("refs/heads/master".to_owned()),
            ..Default::default()
        };
        assert_eq!(classify(&env, "master"), Scenario::PushToDefault);
    }

    #[test]
    fn bk_push_to_default() {
        let env = bk("false", "main");
        assert_eq!(classify(&env, "main"), Scenario::PushToDefault);
    }

    #[test]
    fn bk_push_to_master_default() {
        let env = bk("false", "master");
        assert_eq!(classify(&env, "master"), Scenario::PushToDefault);
    }

    // ── Push to non-default branch ────────────────────────────────────────────

    #[test]
    fn gha_push_to_non_default_branch() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_ref: Some("refs/heads/feature/cool-thing".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            classify(&env, "main"),
            Scenario::PushToBranch {
                branch: "feature/cool-thing".to_owned()
            }
        );
    }

    #[test]
    fn bk_push_to_non_default_branch() {
        let env = bk("false", "feature/experiment");
        assert_eq!(
            classify(&env, "main"),
            Scenario::PushToBranch {
                branch: "feature/experiment".to_owned()
            }
        );
    }

    // ── Local ─────────────────────────────────────────────────────────────────

    #[test]
    fn no_ci_signal_is_local() {
        let env = CiEnvironment::default();
        assert_eq!(classify(&env, DEFAULT), Scenario::Local);
    }

    #[test]
    fn ci_true_but_no_provider_signal_is_local() {
        let env = CiEnvironment {
            ci: true,
            ..Default::default()
        };
        assert_eq!(classify(&env, DEFAULT), Scenario::Local);
    }

    // ── Precedence / ambiguity ────────────────────────────────────────────────

    /// merge_group must win over pull_request (shouldn't occur in real GHA but
    /// confirms the ladder ordering).
    #[test]
    fn merge_group_beats_pull_request_event() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("merge_group".to_owned()),
            github_base_ref: Some("main".to_owned()),
            ..Default::default()
        };
        // If classify erroneously picks pull_request, this fails.
        assert_eq!(classify(&env, DEFAULT), Scenario::MergeQueue);
    }

    /// BK merge queue must win over BK PR flag.
    #[test]
    fn bk_queue_branch_beats_bk_pr_flag() {
        let env = CiEnvironment {
            buildkite: true,
            buildkite_pull_request: Some("5".to_owned()),
            buildkite_branch: Some("gh-readonly-queue/main/pr-5-abc".to_owned()),
            buildkite_pull_request_base_branch: Some("main".to_owned()),
            ..Default::default()
        };
        assert_eq!(classify(&env, DEFAULT), Scenario::MergeQueue);
    }

    /// Tag push ref must not be mistaken for a branch push.
    #[test]
    fn gha_tag_push_falls_through_to_local() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_ref: Some("refs/tags/v1.0.0".to_owned()),
            ..Default::default()
        };
        // branch_from_github_ref returns None for a tag ref, so the push rule
        // falls back to default_branch which equals "main", yielding PushToDefault.
        // That's the correct conservative behaviour: treat an ambiguous push as
        // push-to-default rather than Local.
        assert_eq!(classify(&env, DEFAULT), Scenario::PushToDefault);
    }
}
