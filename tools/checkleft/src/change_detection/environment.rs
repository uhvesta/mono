use std::path::PathBuf;

use serde::Deserialize;

/// Snapshot of CI environment variables, the only place in checkleft that
/// reads these vars. Pass by value downstream to keep all classification
/// logic pure and table-testable.
#[derive(Debug, Clone, Default)]
pub struct CiEnvironment {
    // -- Buildkite --
    pub buildkite: bool,
    /// `"false"` when not a PR, or the PR number as a string when it is.
    pub buildkite_pull_request: Option<String>,
    pub buildkite_pull_request_base_branch: Option<String>,
    pub buildkite_branch: Option<String>,
    pub buildkite_commit: Option<String>,
    pub buildkite_pipeline_default_branch: Option<String>,
    /// Remote URL of the pipeline repository (e.g. `git@github.com:org/repo.git`).
    /// Set by Buildkite as `BUILDKITE_REPO`.
    pub buildkite_repo: Option<String>,

    // -- GitHub Actions --
    pub github_actions: bool,
    /// `pull_request`, `push`, `merge_group`, etc.
    pub github_event_name: Option<String>,
    /// Base branch ref for pull_request events.
    pub github_base_ref: Option<String>,
    /// Head branch ref for pull_request events.
    pub github_head_ref: Option<String>,
    /// Full ref, e.g. `refs/heads/main` for push events.
    pub github_ref: Option<String>,
    pub github_sha: Option<String>,
    /// `owner/repo` slug, set by GHA as `GITHUB_REPOSITORY`.
    pub github_repository: Option<String>,
    /// Path to the JSON event payload file; parsed lazily by
    /// [`CiEnvironment::read_github_event_payload`].
    pub github_event_path: Option<PathBuf>,

    // -- Generic --
    pub ci: bool,
}

impl CiEnvironment {
    pub fn from_env() -> Self {
        let get = |var: &str| std::env::var(var).ok().filter(|v| !v.is_empty());
        let flag = |var: &str| matches!(std::env::var(var).as_deref(), Ok("true") | Ok("1"));

        Self {
            buildkite: flag("BUILDKITE"),
            buildkite_pull_request: get("BUILDKITE_PULL_REQUEST"),
            buildkite_pull_request_base_branch: get("BUILDKITE_PULL_REQUEST_BASE_BRANCH"),
            buildkite_branch: get("BUILDKITE_BRANCH"),
            buildkite_commit: get("BUILDKITE_COMMIT"),
            buildkite_pipeline_default_branch: get("BUILDKITE_PIPELINE_DEFAULT_BRANCH"),
            buildkite_repo: get("BUILDKITE_REPO"),

            github_actions: flag("GITHUB_ACTIONS"),
            github_event_name: get("GITHUB_EVENT_NAME"),
            github_base_ref: get("GITHUB_BASE_REF"),
            github_head_ref: get("GITHUB_HEAD_REF"),
            github_ref: get("GITHUB_REF"),
            github_sha: get("GITHUB_SHA"),
            github_repository: get("GITHUB_REPOSITORY"),
            github_event_path: get("GITHUB_EVENT_PATH").map(PathBuf::from),

            ci: flag("CI"),
        }
    }

    /// Read and parse the GitHub event payload JSON from `GITHUB_EVENT_PATH`,
    /// if available. All fields are treated as optional to tolerate schema
    /// evolution and unexpected event types.
    pub fn read_github_event_payload(&self) -> Option<GithubEventPayload> {
        let path = self.github_event_path.as_ref()?;
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

/// Typed subset of the GitHub event payload. Every field is `Option` so that
/// missing or unknown fields never cause a parse failure.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GithubEventPayload {
    #[serde(default)]
    pub merge_group: Option<MergeGroupPayload>,
    #[serde(default)]
    pub repository: Option<RepositoryPayload>,
    #[serde(default)]
    pub pull_request: Option<PullRequestPayload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeGroupPayload {
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepositoryPayload {
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestPayload {
    pub base: Option<PrBasePayload>,
    pub head: Option<PrHeadPayload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrBasePayload {
    #[serde(rename = "ref")]
    pub branch_ref: Option<String>,
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrHeadPayload {
    #[serde(rename = "ref")]
    pub branch_ref: Option<String>,
    pub sha: Option<String>,
}

/// Resolve the `owner/repo` slug for GitHub API calls.
///
/// Priority (first non-empty wins):
/// 1. `GITHUB_REPOSITORY` — set natively by GitHub Actions; already an `owner/repo` slug.
/// 2. `checks_repository` — the explicit `CHECKS_REPOSITORY` override (read by the caller
///    so this function stays pure).
/// 3. `vcs_slug` — derived from the `origin` remote URL by [`crate::vcs::Vcs::remote_repo_slug`]
///    (also provided by the caller to keep this function pure and table-testable).
pub fn resolve_owner_repo(
    env: &CiEnvironment,
    checks_repository: Option<&str>,
    vcs_slug: Option<&str>,
) -> Option<String> {
    env.github_repository
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| checks_repository.filter(|s| !s.is_empty()))
        .or_else(|| vcs_slug.filter(|s| !s.is_empty()))
        .map(str::to_owned)
}

/// Resolve the head commit SHA for the current CI run.
///
/// Selection rules (first match wins):
///
/// **GitHub Actions**
/// - `pull_request` event: uses `pull_request.head.sha` from the event payload,
///   because `GITHUB_SHA` is the test-merge commit SHA, not the PR head.
/// - All other GHA events: uses `GITHUB_SHA` directly.
///
/// **Buildkite**
/// - Uses `BUILDKITE_COMMIT`.
///
/// Returns `None` when no CI signal is present (local runs).
pub fn resolve_head_sha(env: &CiEnvironment, payload: Option<&GithubEventPayload>) -> Option<String> {
    if env.github_actions {
        if env.github_event_name.as_deref() == Some("pull_request") {
            return payload
                .and_then(|p| p.pull_request.as_ref())
                .and_then(|pr| pr.head.as_ref())
                .and_then(|h| h.sha.clone())
                .filter(|s| !s.is_empty());
        }
        return env.github_sha.clone().filter(|s| !s.is_empty());
    }

    if env.buildkite {
        return env.buildkite_commit.clone().filter(|s| !s.is_empty());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn env_with_event_path(json: &str) -> CiEnvironment {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let path = f.path().to_owned();
        // keep file alive long enough for the test
        std::mem::forget(f);
        CiEnvironment {
            github_event_path: Some(path),
            ..Default::default()
        }
    }

    #[test]
    fn from_env_defaults_when_vars_absent() {
        // Construct manually to avoid inheriting the test runner's env.
        let env = CiEnvironment::default();
        assert!(!env.buildkite);
        assert!(!env.github_actions);
        assert!(!env.ci);
        assert!(env.github_event_name.is_none());
    }

    #[test]
    fn reads_merge_group_payload() {
        let json = r#"{
            "merge_group": {
                "base_sha": "abc123",
                "head_sha": "def456",
                "base_ref": "refs/heads/main"
            }
        }"#;
        let env = env_with_event_path(json);
        let payload = env.read_github_event_payload().unwrap();
        let mg = payload.merge_group.unwrap();
        assert_eq!(mg.base_sha.as_deref(), Some("abc123"));
        assert_eq!(mg.head_sha.as_deref(), Some("def456"));
        assert_eq!(mg.base_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn reads_repository_default_branch() {
        let json = r#"{"repository": {"default_branch": "master"}}"#;
        let env = env_with_event_path(json);
        let payload = env.read_github_event_payload().unwrap();
        assert_eq!(payload.repository.unwrap().default_branch.as_deref(), Some("master"));
    }

    #[test]
    fn tolerates_unknown_fields_in_payload() {
        let json = r#"{"unknown_top_level": true, "merge_group": {"base_sha": "aa", "future_field": 42}}"#;
        let env = env_with_event_path(json);
        let payload = env.read_github_event_payload().unwrap();
        assert_eq!(payload.merge_group.unwrap().base_sha.as_deref(), Some("aa"));
    }

    #[test]
    fn returns_none_when_path_absent() {
        let env = CiEnvironment::default();
        assert!(env.read_github_event_payload().is_none());
    }

    #[test]
    fn returns_none_for_invalid_json() {
        let env = CiEnvironment {
            github_event_path: Some(PathBuf::from("/nonexistent/path/event.json")),
            ..Default::default()
        };
        assert!(env.read_github_event_payload().is_none());
    }

    #[test]
    fn reads_pull_request_head_sha_from_payload() {
        let json = r#"{
            "pull_request": {
                "base": {"ref": "main", "sha": "base000"},
                "head": {"ref": "feature/x", "sha": "head111"}
            }
        }"#;
        let env = env_with_event_path(json);
        let payload = env.read_github_event_payload().unwrap();
        let pr = payload.pull_request.unwrap();
        assert_eq!(pr.base.as_ref().and_then(|b| b.sha.as_deref()), Some("base000"));
        assert_eq!(pr.head.as_ref().and_then(|h| h.sha.as_deref()), Some("head111"));
        assert_eq!(
            pr.head.as_ref().and_then(|h| h.branch_ref.as_deref()),
            Some("feature/x")
        );
    }

    // ── resolve_owner_repo ────────────────────────────────────────────────────

    #[test]
    fn resolve_owner_repo_prefers_github_repository() {
        let env = CiEnvironment {
            github_repository: Some("org/from-gha".to_owned()),
            ..Default::default()
        };
        let result = resolve_owner_repo(&env, Some("org/checks-override"), Some("org/from-vcs"));
        assert_eq!(result.as_deref(), Some("org/from-gha"));
    }

    #[test]
    fn resolve_owner_repo_falls_back_to_checks_repository() {
        let env = CiEnvironment {
            github_repository: None,
            ..Default::default()
        };
        let result = resolve_owner_repo(&env, Some("org/checks-override"), Some("org/from-vcs"));
        assert_eq!(result.as_deref(), Some("org/checks-override"));
    }

    #[test]
    fn resolve_owner_repo_falls_back_to_vcs_slug() {
        let env = CiEnvironment {
            github_repository: None,
            ..Default::default()
        };
        let result = resolve_owner_repo(&env, None, Some("org/from-vcs"));
        assert_eq!(result.as_deref(), Some("org/from-vcs"));
    }

    #[test]
    fn resolve_owner_repo_returns_none_when_all_absent() {
        let env = CiEnvironment::default();
        assert!(resolve_owner_repo(&env, None, None).is_none());
    }

    #[test]
    fn resolve_owner_repo_skips_empty_github_repository() {
        let env = CiEnvironment {
            github_repository: Some(String::new()),
            ..Default::default()
        };
        let result = resolve_owner_repo(&env, Some("org/fallback"), None);
        assert_eq!(result.as_deref(), Some("org/fallback"));
    }

    #[test]
    fn resolve_owner_repo_skips_empty_checks_repository() {
        let env = CiEnvironment::default();
        let result = resolve_owner_repo(&env, Some(""), Some("org/from-vcs"));
        assert_eq!(result.as_deref(), Some("org/from-vcs"));
    }

    // ── resolve_head_sha ──────────────────────────────────────────────────────

    fn pr_payload_with_head(head_sha: &str) -> GithubEventPayload {
        GithubEventPayload {
            pull_request: Some(PullRequestPayload {
                base: None,
                head: Some(PrHeadPayload {
                    branch_ref: Some("feature/x".to_owned()),
                    sha: Some(head_sha.to_owned()),
                }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_head_sha_gha_pull_request_uses_payload_head() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_sha: Some("merge-commit-sha".to_owned()),
            ..Default::default()
        };
        let payload = pr_payload_with_head("pr-head-sha");
        let result = resolve_head_sha(&env, Some(&payload));
        assert_eq!(result.as_deref(), Some("pr-head-sha"));
    }

    #[test]
    fn resolve_head_sha_gha_push_uses_github_sha() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_sha: Some("push-sha".to_owned()),
            ..Default::default()
        };
        let result = resolve_head_sha(&env, None);
        assert_eq!(result.as_deref(), Some("push-sha"));
    }

    #[test]
    fn resolve_head_sha_gha_merge_group_uses_github_sha() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("merge_group".to_owned()),
            github_sha: Some("merge-queue-sha".to_owned()),
            ..Default::default()
        };
        let result = resolve_head_sha(&env, None);
        assert_eq!(result.as_deref(), Some("merge-queue-sha"));
    }

    #[test]
    fn resolve_head_sha_buildkite_uses_buildkite_commit() {
        let env = CiEnvironment {
            buildkite: true,
            buildkite_commit: Some("bk-commit-sha".to_owned()),
            ..Default::default()
        };
        let result = resolve_head_sha(&env, None);
        assert_eq!(result.as_deref(), Some("bk-commit-sha"));
    }

    #[test]
    fn resolve_head_sha_local_returns_none() {
        let env = CiEnvironment::default();
        assert!(resolve_head_sha(&env, None).is_none());
    }

    #[test]
    fn resolve_head_sha_gha_pr_missing_payload_returns_none() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_sha: Some("merge-sha".to_owned()),
            ..Default::default()
        };
        // No payload provided — should return None rather than falling through to github_sha.
        let result = resolve_head_sha(&env, None);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_head_sha_gha_pr_payload_head_sha_missing_returns_none() {
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("pull_request".to_owned()),
            github_sha: Some("merge-sha".to_owned()),
            ..Default::default()
        };
        let payload = GithubEventPayload {
            pull_request: Some(PullRequestPayload { base: None, head: None }),
            ..Default::default()
        };
        let result = resolve_head_sha(&env, Some(&payload));
        assert!(result.is_none());
    }

    #[test]
    fn resolve_head_sha_gha_takes_priority_over_buildkite() {
        // When both signals are present (unusual in practice), GHA wins.
        let env = CiEnvironment {
            github_actions: true,
            github_event_name: Some("push".to_owned()),
            github_sha: Some("gha-sha".to_owned()),
            buildkite: true,
            buildkite_commit: Some("bk-sha".to_owned()),
            ..Default::default()
        };
        let result = resolve_head_sha(&env, None);
        assert_eq!(result.as_deref(), Some("gha-sha"));
    }
}
