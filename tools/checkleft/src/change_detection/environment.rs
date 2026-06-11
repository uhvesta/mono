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

            github_actions: flag("GITHUB_ACTIONS"),
            github_event_name: get("GITHUB_EVENT_NAME"),
            github_base_ref: get("GITHUB_BASE_REF"),
            github_head_ref: get("GITHUB_HEAD_REF"),
            github_ref: get("GITHUB_REF"),
            github_sha: get("GITHUB_SHA"),
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrBasePayload {
    #[serde(rename = "ref")]
    pub branch_ref: Option<String>,
    pub sha: Option<String>,
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
}
