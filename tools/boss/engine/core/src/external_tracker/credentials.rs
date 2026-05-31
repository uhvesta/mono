//! Credential resolution for external trackers.
//!
//! Two resolvers are provided:
//!
//! - [`GhAuthStatusResolver`] — v1: delegates to `gh auth status` and returns
//!   an ambient credential (no stored token).
//! - [`KeychainOAuthResolver`] — v2: checks the OS keychain for a stored OAuth
//!   token first, and falls back to [`GhAuthStatusResolver`] when none is
//!   present.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

use super::TrackerCredential;
use super::github_oauth::KeychainTokenStore;

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

// ── KeychainOAuthResolver ─────────────────────────────────────────────────────

/// Resolves a [`TrackerCredential`] by consulting the OS keychain first.
///
/// When a stored OAuth token is present for GitHub, it is returned directly.
/// When absent (or if the keychain is unavailable), the resolver delegates to
/// [`GhAuthStatusResolver`] which relies on the ambient `gh` login.
///
/// # Keychain errors
/// A keychain access failure is treated as "no stored token" — a warning is
/// logged and the fallback resolver is used.  This keeps sync working on
/// machines where the engine process cannot access the keychain (e.g. headless
/// CI without a login keychain unlocked).
pub struct KeychainOAuthResolver {
    store: KeychainTokenStore,
    fallback: GhAuthStatusResolver,
}

impl KeychainOAuthResolver {
    pub fn new(store: KeychainTokenStore) -> Self {
        Self { store, fallback: GhAuthStatusResolver::default() }
    }

    /// Test constructor: supply a custom fallback resolver (e.g. one pointing
    /// at a fake `gh` binary).
    #[cfg(test)]
    pub(crate) fn with_fallback(store: KeychainTokenStore, fallback: GhAuthStatusResolver) -> Self {
        Self { store, fallback }
    }
}

#[async_trait]
impl TrackerCredentialResolver for KeychainOAuthResolver {
    async fn resolve(
        &self,
        kind: &str,
        config: &serde_json::Value,
    ) -> Result<TrackerCredential, TrackerCredentialError> {
        if kind == "github" {
            match self.store.get() {
                Ok(Some(record)) => return Ok(TrackerCredential { token: record.token }),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "boss_engine::external_tracker::credentials",
                        error = %e,
                        "keychain unavailable; falling back to ambient gh for GitHub sync"
                    );
                }
            }
        }
        self.fallback.resolve(kind, config).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use crate::external_tracker::github_oauth::{FakeStore, KeychainTokenStore, TokenRecord};

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

    fn sample_record() -> TokenRecord {
        TokenRecord {
            token: "gho_resolver_test_token".to_owned(),
            login: "testuser".to_owned(),
            granted_scopes: vec!["repo".to_owned()],
            obtained_at: 0,
        }
    }

    #[tokio::test]
    async fn keychain_resolver_returns_stored_token_when_present() {
        let store = KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record()));
        let resolver = KeychainOAuthResolver::new(store);
        let cred = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect("should succeed");
        assert_eq!(cred.token, "gho_resolver_test_token");
    }

    #[tokio::test]
    async fn keychain_resolver_falls_back_to_ambient_when_no_stored_token() {
        let fake_gh = make_fake_gh("exit 0");
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        let fallback = GhAuthStatusResolver::with_gh_binary(&*fake_gh);
        let resolver = KeychainOAuthResolver::with_fallback(store, fallback);
        let cred = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect("should succeed");
        assert_eq!(cred.token, ""); // ambient credential
    }

    #[tokio::test]
    async fn keychain_resolver_falls_back_and_propagates_gh_error_when_gh_fails() {
        let fake_gh = make_fake_gh("echo 'not logged in' >&2; exit 1");
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        let fallback = GhAuthStatusResolver::with_gh_binary(&*fake_gh);
        let resolver = KeychainOAuthResolver::with_fallback(store, fallback);
        let err = resolver
            .resolve("github", &serde_json::Value::Null)
            .await
            .expect_err("should fail when both keychain and gh are unavailable");
        assert!(
            matches!(err, TrackerCredentialError::AuthFailed { .. }),
            "expected AuthFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn keychain_resolver_delegates_non_github_kind_to_fallback() {
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        let resolver = KeychainOAuthResolver::new(store);
        let err = resolver
            .resolve("jira", &serde_json::Value::Null)
            .await
            .expect_err("should fail for non-github kind");
        assert!(
            matches!(err, TrackerCredentialError::UnsupportedKind(ref k) if k == "jira"),
            "expected UnsupportedKind(jira), got {err:?}"
        );
    }
}
