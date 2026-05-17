//! Credential resolution for external trackers.
//!
//! The v1 implementation relies on the ambient `gh` login: `gh auth status`
//! confirms the user is authenticated; every subsequent `gh api` call inherits
//! that credential implicitly. No PAT is stored in Boss state.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

use super::TrackerCredential;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum TrackerCredentialError {
    #[error("gh auth status failed for {host}: {detail}")]
    AuthFailed { host: String, detail: String },
    #[error("unsupported tracker kind: {0}")]
    UnsupportedKind(String),
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Resolves a [`TrackerCredential`] for a given tracker kind and config.
///
/// v1 ships one impl: [`GhAuthStatusResolver`], which runs `gh auth status`
/// and returns an ambient credential. Future impls can read a PAT from
/// the OS keychain without changing the engine's call sites.
#[async_trait]
pub trait TrackerCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        kind: &str,
        config: &serde_json::Value,
    ) -> Result<TrackerCredential, TrackerCredentialError>;
}

// ── GhAuthStatusResolver ──────────────────────────────────────────────────────

/// Default credential resolver that delegates to `gh auth status`.
///
/// On success the returned credential is ambient (empty token): every
/// subsequent `gh api` call will use the user's existing `gh` login.
/// On failure, a structured warning is logged so T11's attention-item hook
/// can surface it; the caller gets `AuthFailed`.
pub struct GhAuthStatusResolver {
    gh_binary: PathBuf,
}

impl Default for GhAuthStatusResolver {
    fn default() -> Self {
        Self { gh_binary: PathBuf::from("gh") }
    }
}

impl GhAuthStatusResolver {
    /// Use a custom `gh` binary path. Intended for test injection.
    pub fn with_gh_binary(path: impl Into<PathBuf>) -> Self {
        Self { gh_binary: path.into() }
    }

    fn gh(&self) -> &Path {
        &self.gh_binary
    }
}

#[async_trait]
impl TrackerCredentialResolver for GhAuthStatusResolver {
    async fn resolve(
        &self,
        kind: &str,
        _config: &serde_json::Value,
    ) -> Result<TrackerCredential, TrackerCredentialError> {
        match kind {
            "github" => {
                let host = "github.com";
                let output = Command::new(self.gh())
                    .args(["auth", "status", "--hostname", host])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| TrackerCredentialError::AuthFailed {
                        host: host.to_owned(),
                        detail: e.to_string(),
                    })?;

                if output.status.success() {
                    return Ok(TrackerCredential::ambient());
                }

                let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                // Structured log marker; T11's attention-item hook watches this target.
                tracing::warn!(
                    target: "boss_engine::external_tracker::credentials",
                    %host,
                    %detail,
                    "gh auth status failed; GitHub-bound products will be skipped until auth is restored"
                );
                Err(TrackerCredentialError::AuthFailed { host: host.to_owned(), detail })
            }
            other => Err(TrackerCredentialError::UnsupportedKind(other.to_owned())),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Write a shell script to a temp file, close the write fd, and make it executable.
    ///
    /// Returning `TempPath` (not `NamedTempFile`) is intentional: `NamedTempFile` holds
    /// the file open for writing, and on Linux exec fails with ETXTBSY if any fd with
    /// O_WRONLY is open against the target. `into_temp_path()` closes the write fd while
    /// keeping the file on disk until the returned value is dropped.
    fn make_fake_gh(script: &str) -> tempfile::TempPath {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        write!(f, "#!/bin/sh\n{script}").expect("write script");
        let mut perms = f.as_file().metadata().expect("metadata").permissions();
        perms.set_mode(0o755);
        f.as_file().set_permissions(perms).expect("set permissions");
        f.into_temp_path()
    }

    #[tokio::test]
    async fn resolves_ambient_credential_when_gh_auth_succeeds() {
        let fake = make_fake_gh("exit 0");
        let resolver = GhAuthStatusResolver::with_gh_binary(&*fake);
        let cred = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect("should succeed");
        assert_eq!(cred.token, "");
    }

    #[tokio::test]
    async fn returns_auth_failed_when_gh_exits_nonzero() {
        let fake = make_fake_gh("echo 'not logged in to github.com' >&2; exit 1");
        let resolver = GhAuthStatusResolver::with_gh_binary(&*fake);
        let err = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, TrackerCredentialError::AuthFailed { .. }),
            "expected AuthFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn returns_auth_failed_when_gh_binary_is_missing() {
        let resolver = GhAuthStatusResolver::with_gh_binary("/nonexistent/bin/gh_fake_12345");
        let err = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, TrackerCredentialError::AuthFailed { .. }),
            "expected AuthFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn returns_unsupported_kind_for_non_github_tracker() {
        let resolver = GhAuthStatusResolver::default();
        let err = resolver
            .resolve("jira", &serde_json::Value::Null)
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, TrackerCredentialError::UnsupportedKind(ref k) if k == "jira"),
            "expected UnsupportedKind(jira), got {err:?}"
        );
    }
}
