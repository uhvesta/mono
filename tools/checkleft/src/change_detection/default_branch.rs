use tracing::warn;

use super::environment::CiEnvironment;

/// Injectable interface for probing whether a ref exists and for resolving
/// symbolic refs. Abstracted so the resolution ladder is unit-testable without
/// a real git repo.
pub trait RefProber {
    /// Return the short branch name that `refname` (a symbolic ref like
    /// `refs/remotes/origin/HEAD`) points to, or `None` if it cannot be
    /// resolved.
    fn resolve_symbolic_ref(&self, refname: &str) -> Option<String>;

    /// Return `true` if `refname` (e.g. `origin/main`) exists and is reachable.
    fn ref_exists(&self, refname: &str) -> bool;
}

/// Resolve the default/integration branch name using the ordered ladder defined
/// in the design:
///
/// 1. `override_branch` — explicit `--default-branch` CLI flag.
/// 2. CI hint — `BUILDKITE_PIPELINE_DEFAULT_BRANCH`, GHA
///    `repository.default_branch` from the event payload, or the target segment
///    of a `gh-readonly-queue/<target>/…` BK branch.
/// 3. `git symbolic-ref refs/remotes/origin/HEAD` (the remote's default).
/// 4. Probe `origin/main` → `origin/master` → local `main` → local `master`.
/// 5. Fallback: `"main"` with a warning.
pub fn resolve_default_branch(
    env: &CiEnvironment,
    prober: &dyn RefProber,
    override_branch: Option<&str>,
) -> String {
    // 1. Explicit override.
    if let Some(branch) = override_branch.filter(|s| !s.is_empty()) {
        return branch.to_owned();
    }

    // 2a. Buildkite explicit default branch.
    if let Some(branch) = env
        .buildkite_pipeline_default_branch
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        return branch.to_owned();
    }

    // 2b. BK merge queue: target branch is encoded in the branch name as
    //     `gh-readonly-queue/<target>/…`.
    if let Some(bk_branch) = env.buildkite_branch.as_deref()
        && let Some(rest) = bk_branch.strip_prefix("gh-readonly-queue/")
            && let Some(target) = rest.split('/').next().filter(|s| !s.is_empty()) {
                return target.to_owned();
            }

    // 2c. GHA: `repository.default_branch` from the event payload.
    if env.github_actions
        && let Some(payload) = env.read_github_event_payload()
            && let Some(branch) = payload
                .repository
                .and_then(|r| r.default_branch)
                .filter(|s| !s.is_empty())
            {
                return branch;
            }

    // 3. Remote symbolic ref.
    if let Some(branch) = prober
        .resolve_symbolic_ref("refs/remotes/origin/HEAD")
        .and_then(|r| short_branch_from_remote_ref(&r))
    {
        return branch;
    }

    // 4. Probe candidates in preference order.
    for candidate in &["origin/main", "origin/master", "main", "master"] {
        if prober.ref_exists(candidate) {
            return candidate
                .strip_prefix("origin/")
                .unwrap_or(candidate)
                .to_owned();
        }
    }

    // 5. Fallback.
    warn!("could not determine default branch; falling back to 'main'");
    "main".to_owned()
}

/// Strip `refs/remotes/origin/` or `refs/heads/` prefix, returning just the
/// short branch name.
fn short_branch_from_remote_ref(symbolic_ref: &str) -> Option<String> {
    let stripped = symbolic_ref
        .trim()
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| symbolic_ref.trim().strip_prefix("refs/heads/"))
        .unwrap_or(symbolic_ref.trim());
    let name = stripped.trim();
    if name.is_empty() { None } else { Some(name.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProber {
        symbolic: Option<String>,
        existing: Vec<&'static str>,
    }

    impl RefProber for StubProber {
        fn resolve_symbolic_ref(&self, _refname: &str) -> Option<String> {
            self.symbolic.clone()
        }
        fn ref_exists(&self, refname: &str) -> bool {
            self.existing.contains(&refname)
        }
    }

    fn no_ci() -> CiEnvironment {
        CiEnvironment::default()
    }

    fn bk_env(pipeline_default: Option<&str>, branch: Option<&str>) -> CiEnvironment {
        CiEnvironment {
            buildkite: true,
            buildkite_pipeline_default_branch: pipeline_default.map(str::to_owned),
            buildkite_branch: branch.map(str::to_owned),
            ..Default::default()
        }
    }

    fn prober(symbolic: Option<&str>, existing: Vec<&'static str>) -> StubProber {
        StubProber {
            symbolic: symbolic.map(str::to_owned),
            existing,
        }
    }

    #[test]
    fn explicit_override_wins() {
        let p = prober(Some("refs/remotes/origin/HEAD -> refs/remotes/origin/master"), vec![]);
        assert_eq!(
            resolve_default_branch(&no_ci(), &p, Some("develop")),
            "develop"
        );
    }

    #[test]
    fn buildkite_pipeline_default_branch() {
        let env = bk_env(Some("master"), None);
        let p = prober(None, vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }

    #[test]
    fn buildkite_merge_queue_target_branch() {
        let env = bk_env(None, Some("gh-readonly-queue/main/pr-42-sha"));
        let p = prober(None, vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn buildkite_merge_queue_master_target() {
        let env = bk_env(None, Some("gh-readonly-queue/master/pr-7-sha"));
        let p = prober(None, vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }

    #[test]
    fn symbolic_ref_resolves_to_main() {
        let env = no_ci();
        let p = prober(Some("refs/remotes/origin/main"), vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn symbolic_ref_resolves_to_master() {
        let env = no_ci();
        let p = prober(Some("refs/remotes/origin/master"), vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }

    #[test]
    fn probe_origin_main_exists() {
        let env = no_ci();
        let p = prober(None, vec!["origin/main"]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn probe_origin_master_when_main_absent() {
        let env = no_ci();
        let p = prober(None, vec!["origin/master"]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }

    #[test]
    fn probe_local_main_when_remote_absent() {
        let env = no_ci();
        let p = prober(None, vec!["main"]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn probe_local_master_when_all_remote_absent() {
        let env = no_ci();
        let p = prober(None, vec!["master"]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }

    #[test]
    fn fallback_to_main_when_nothing_resolves() {
        let env = no_ci();
        let p = prober(None, vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn override_beats_buildkite_pipeline_default() {
        let env = bk_env(Some("master"), None);
        let p = prober(None, vec![]);
        assert_eq!(
            resolve_default_branch(&env, &p, Some("develop")),
            "develop"
        );
    }

    #[test]
    fn symbolic_ref_with_full_heads_prefix() {
        let env = no_ci();
        let p = prober(Some("refs/heads/main"), vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "main");
    }

    #[test]
    fn bk_pipeline_default_beats_symbolic_ref() {
        // Step 2a must fire before step 3.
        let env = bk_env(Some("master"), None);
        let p = prober(Some("refs/remotes/origin/main"), vec![]);
        assert_eq!(resolve_default_branch(&env, &p, None), "master");
    }
}
