//! Creation-time cube repo-slug resolution.
//!
//! `boss task create --repo <slug>` / `boss chore create --repo <slug>`
//! let an operator name a repo by its registered cube slug (e.g.
//! `bduff`) instead of a full git origin URL. The durable work-item row
//! must carry a *dispatchable* origin URL, not the slug: at dispatch the
//! engine hands `repo_remote_url` straight to `cube repo ensure --origin`,
//! and cube rejects a bare slug that is already registered with a
//! non-empty origin (`repo `bduff` is already configured for origin …`).
//!
//! So at creation time we look the slug up against cube's registry and
//! rewrite the field to the canonical origin. The chore row stays
//! introspectable and the dispatch path stays dumb — it only ever sees
//! URLs. See spinyfin/mono#861.
//!
//! Best-effort: a value that already looks like a URL / scp remote /
//! path, a slug cube doesn't know, or a failed registry round-trip all
//! leave the field untouched and fall through to the pre-existing
//! behaviour.

use std::sync::Arc;

use crate::coordinator::{CubeClient, CubeRepoSummary};

/// True when `value` is a bare cube repo slug rather than a git remote
/// URL, scp-style remote, or filesystem path. URLs and remotes always
/// carry at least one of `:` (scheme / scp host separator), `/` (path),
/// or `@` (user); a slug like `bduff` carries none of those and no
/// whitespace.
pub fn is_bare_repo_slug(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| !matches!(c, ':' | '/' | '@') && !c.is_whitespace())
}

/// Rewrite a single `repo_remote_url` field in place: when it holds a
/// bare slug that matches a `repo_id` in `repos`, replace it with that
/// repo's canonical origin. No-op for `None`, for values that already
/// look like a URL, and for slugs cube doesn't know.
pub fn resolve_slug_in_place(repos: &[CubeRepoSummary], field: &mut Option<String>) {
    let Some(raw) = field.as_deref() else {
        return;
    };
    let trimmed = raw.trim();
    if !is_bare_repo_slug(trimmed) {
        return;
    }
    if let Some(found) = repos.iter().find(|repo| repo.repo_id == trimmed) {
        *field = Some(found.origin.clone());
    }
}

/// Resolve every bare slug among `fields` to its canonical cube origin,
/// fetching the registry at most once. Skips the round-trip entirely
/// when nothing looks like a slug, so a URL-valued (or empty) `--repo`
/// costs no extra cube call. Best-effort: a registry failure logs at
/// WARN and leaves the fields verbatim.
pub async fn resolve_repo_slugs(
    cube_client: &Arc<dyn CubeClient>,
    fields: &mut [&mut Option<String>],
) {
    let any_slug = fields
        .iter()
        .any(|field| field.as_deref().is_some_and(is_bare_repo_slug));
    if !any_slug {
        return;
    }
    let repos = match cube_client.list_repos().await {
        Ok(repos) => repos,
        Err(err) => {
            tracing::warn!(
                ?err,
                "repo-slug resolution: `cube repo list` failed; storing --repo value verbatim"
            );
            return;
        }
    };
    for field in fields.iter_mut() {
        resolve_slug_in_place(&repos, field);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(repo_id: &str, origin: &str) -> CubeRepoSummary {
        CubeRepoSummary {
            repo_id: repo_id.to_owned(),
            origin: origin.to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: PathBuf::from("/tmp"),
            workspace_prefix: format!("{repo_id}-"),
            source: None,
        }
    }

    #[test]
    fn bare_slug_recognises_plain_names_and_rejects_urls() {
        assert!(is_bare_repo_slug("bduff"));
        assert!(is_bare_repo_slug("my-repo"));
        assert!(is_bare_repo_slug("repo.with.dots"));
        assert!(is_bare_repo_slug("  bduff  ")); // trimmed

        assert!(!is_bare_repo_slug(""));
        assert!(!is_bare_repo_slug("   "));
        assert!(!is_bare_repo_slug("git@github.com:linkedin-sandbox/bduff.git"));
        assert!(!is_bare_repo_slug("https://github.com/foo/bar.git"));
        assert!(!is_bare_repo_slug("foo/bar"));
        assert!(!is_bare_repo_slug("org-132020694@github.com:ls/bduff.git"));
        assert!(!is_bare_repo_slug("two words"));
    }

    #[test]
    fn resolves_known_slug_to_origin() {
        let repos = vec![repo(
            "bduff",
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
        )];
        let mut field = Some("bduff".to_owned());
        resolve_slug_in_place(&repos, &mut field);
        assert_eq!(
            field.as_deref(),
            Some("org-132020694@github.com:linkedin-sandbox/bduff.git")
        );
    }

    #[test]
    fn resolves_known_slug_with_surrounding_whitespace() {
        let repos = vec![repo("bduff", "git@github.com:ls/bduff.git")];
        let mut field = Some("  bduff ".to_owned());
        resolve_slug_in_place(&repos, &mut field);
        assert_eq!(field.as_deref(), Some("git@github.com:ls/bduff.git"));
    }

    #[test]
    fn leaves_unknown_slug_untouched() {
        let repos = vec![repo("bduff", "git@github.com:ls/bduff.git")];
        let mut field = Some("nope".to_owned());
        resolve_slug_in_place(&repos, &mut field);
        assert_eq!(field.as_deref(), Some("nope"));
    }

    #[test]
    fn leaves_url_value_untouched_even_when_registered() {
        // A full URL is never treated as a slug, so it is passed through
        // verbatim regardless of registry contents.
        let repos = vec![repo(
            "bduff",
            "git@github.com:ls/bduff.git",
        )];
        let mut field = Some("git@github.com:ls/bduff.git".to_owned());
        resolve_slug_in_place(&repos, &mut field);
        assert_eq!(field.as_deref(), Some("git@github.com:ls/bduff.git"));
    }

    #[test]
    fn leaves_none_untouched() {
        let repos = vec![repo("bduff", "git@github.com:ls/bduff.git")];
        let mut field: Option<String> = None;
        resolve_slug_in_place(&repos, &mut field);
        assert_eq!(field, None);
    }

    // ── async wrapper: the create-handler entry point ────────────────────

    use crate::coordinator::{
        CubeChangeHandle, CubeRepoHandle, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Minimal `CubeClient` double: serves a fixed repo snapshot (or an
    /// error) from `list_repos` and counts the calls. Every other trait
    /// method is unreachable in these tests.
    struct FakeCube {
        repos: Result<Vec<CubeRepoSummary>>,
        list_calls: AtomicUsize,
    }

    impl FakeCube {
        fn with_repos(repos: Vec<CubeRepoSummary>) -> Self {
            Self {
                repos: Ok(repos),
                list_calls: AtomicUsize::new(0),
            }
        }
        fn failing() -> Self {
            Self {
                repos: Err(anyhow!("cube unreachable")),
                list_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CubeClient for FakeCube {
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            match &self.repos {
                Ok(repos) => Ok(repos.clone()),
                Err(err) => Err(anyhow!("{err}")),
            }
        }
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unreachable!()
        }
        async fn lease_workspace(
            &self,
            _repo_id: &str,
            _task: &str,
            _prefer: Option<&str>,
            _allow_dirty: bool,
        ) -> Result<CubeWorkspaceLease> {
            unreachable!()
        }
        async fn create_change(
            &self,
            _workspace_path: &PathBuf,
            _title: &str,
        ) -> Result<CubeChangeHandle> {
            unreachable!()
        }
        async fn release_workspace(&self, _lease_id: &str) -> Result<()> {
            unreachable!()
        }
        async fn workspace_status(&self, _workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
            unreachable!()
        }
        async fn heartbeat_lease(&self, _lease_id: &str, _ttl: Option<u64>) -> Result<()> {
            unreachable!()
        }
        async fn force_release_lease(&self, _lease_id: &str, _reason: Option<&str>) -> Result<()> {
            unreachable!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn resolves_slug_field_via_registry() {
        let cube: Arc<dyn CubeClient> = Arc::new(FakeCube::with_repos(vec![repo(
            "bduff",
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
        )]));
        let mut repo_field = Some("bduff".to_owned());
        resolve_repo_slugs(&cube, &mut [&mut repo_field]).await;
        assert_eq!(
            repo_field.as_deref(),
            Some("org-132020694@github.com:linkedin-sandbox/bduff.git")
        );
    }

    #[tokio::test]
    async fn skips_registry_round_trip_when_no_field_is_a_slug() {
        let fake = Arc::new(FakeCube::with_repos(vec![repo(
            "bduff",
            "git@github.com:ls/bduff.git",
        )]));
        let cube: Arc<dyn CubeClient> = fake.clone();
        // A full URL and a None — neither looks like a bare slug, so cube
        // is never consulted.
        let mut url_field = Some("git@github.com:ls/other.git".to_owned());
        let mut none_field: Option<String> = None;
        resolve_repo_slugs(&cube, &mut [&mut url_field, &mut none_field]).await;
        assert_eq!(url_field.as_deref(), Some("git@github.com:ls/other.git"));
        assert_eq!(none_field, None);
        assert_eq!(fake.list_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn leaves_field_verbatim_when_registry_lookup_fails() {
        let cube: Arc<dyn CubeClient> = Arc::new(FakeCube::failing());
        let mut repo_field = Some("bduff".to_owned());
        resolve_repo_slugs(&cube, &mut [&mut repo_field]).await;
        // Best-effort: a failed `cube repo list` leaves the slug as-is so
        // the create still succeeds (and falls through to the pre-existing
        // dispatch-time behaviour).
        assert_eq!(repo_field.as_deref(), Some("bduff"));
    }

    #[tokio::test]
    async fn resolves_a_mixed_batch_in_one_round_trip() {
        let fake = Arc::new(FakeCube::with_repos(vec![
            repo("bduff", "git@github.com:ls/bduff.git"),
            repo("mono", "git@github.com:spinyfin/mono.git"),
        ]));
        let cube: Arc<dyn CubeClient> = fake.clone();
        let mut a = Some("bduff".to_owned()); // resolved
        let mut b = Some("git@github.com:x/y.git".to_owned()); // URL, untouched
        let mut c = Some("unknown-slug".to_owned()); // slug, no match → untouched
        let mut d = Some("mono".to_owned()); // resolved
        resolve_repo_slugs(&cube, &mut [&mut a, &mut b, &mut c, &mut d]).await;
        assert_eq!(a.as_deref(), Some("git@github.com:ls/bduff.git"));
        assert_eq!(b.as_deref(), Some("git@github.com:x/y.git"));
        assert_eq!(c.as_deref(), Some("unknown-slug"));
        assert_eq!(d.as_deref(), Some("git@github.com:spinyfin/mono.git"));
        // One registry fetch for the whole batch.
        assert_eq!(fake.list_calls.load(Ordering::SeqCst), 1);
    }
}
