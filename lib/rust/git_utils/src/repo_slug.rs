//! Parse git remote URLs into owner/repo slugs and classify bare slugs.
//!
//! This is the superset of the three formerly-diverged copies that lived
//! in `boss-github`, `cube`, and the boss engine. It handles:
//!
//! - plain github.com SSH (`git@github.com:owner/repo.git`) and HTTPS
//!   (`https://github.com/owner/repo`) remotes,
//! - GitHub Enterprise / SSO auth-prefixed SSH remotes
//!   (`org-NNN@github.com:owner/repo.git`),
//! - RFC-3986 `ssh://[user@]host[:port]/path` remotes, and
//! - bare cube reponames (`bduff`) vs bare `owner/name` slugs
//!   (`linkedin-multiproduct/dev-infra`) vs real clone URLs.
//!
//! Two distinct "bare slug" predicates are provided because cube and the
//! engine mean different things by the phrase:
//!
//! - [`is_bare_repo_slug`] — a single bare cube reponame carrying *no*
//!   URL punctuation at all (`bduff`, `my-repo`). Used by the engine's
//!   creation-time slug resolution.
//! - [`is_owner_name_slug`] — a bare `owner/name` GitHub slug that has a
//!   `/` path separator but is not a parseable clone URL. Used by cube's
//!   `repo ensure` idempotency check.

use thiserror::Error;

/// Errors that can occur when parsing a GitHub remote URL.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("not a github.com URL: {0}")]
    NotGithub(String),
    #[error("missing owner segment: {0}")]
    MissingOwner(String),
    #[error("missing repo segment: {0}")]
    MissingRepo(String),
}

/// Parse a github.com remote URL into its borrowed `(owner, repo)` path
/// segments.
///
/// Both remote shapes are accepted:
///
/// - SSH:   `git@github.com:owner/repo.git`
/// - HTTPS: `https://github.com/owner/repo[/...][.git][/]`
///
/// The algorithm trims surrounding whitespace, strips a trailing `/` and a
/// trailing `.git`, splits on the literal `github.com`, trims the leading
/// `:`/`/` host separators, then takes the first two non-empty
/// `/`-delimited segments. Any path components after `owner/repo` (e.g.
/// `/pull/123`) are ignored.
///
/// Returns a granular [`ParseError`] so call sites that surface messages
/// verbatim can do so; callers that only want an `Option` use `.ok()`.
pub fn parse_github_owner_repo(url: &str) -> Result<(&str, &str), ParseError> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let (_, after_host) = trimmed
        .split_once("github.com")
        .ok_or_else(|| ParseError::NotGithub(url.to_owned()))?;
    let after_host = after_host.trim_start_matches([':', '/']);
    let mut parts = after_host.splitn(3, '/');
    let owner = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ParseError::MissingOwner(url.to_owned()))?;
    let repo = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ParseError::MissingRepo(url.to_owned()))?;
    Ok((owner, repo))
}

/// Parse `owner/repo` from a github.com remote URL (SSH or HTTPS),
/// returning the joined slug. Thin owned-`String` wrapper over
/// [`parse_github_owner_repo`] for call sites that want a single value.
pub fn parse_github_slug(remote_url: &str) -> Option<String> {
    let (owner, repo) = parse_github_owner_repo(remote_url).ok()?;
    Some(format!("{owner}/{repo}"))
}

/// Short name of a repo URL: the path basename minus a trailing `.git`.
///
/// Strips protocol + host and takes the final path segment, handling
/// both remote shapes:
///   `git@github.com:foo/bar.git` → `bar`
///   `https://github.com/foo/bar.git` → `bar`
///
/// Pure-string parse — no registry lookup. Used to match a `--repo`
/// short-name selector against a resolved repo URL.
pub fn short_name_for(url: &str) -> &str {
    let after_slash = url.rsplit('/').next().unwrap_or(url);
    let after_colon = after_slash.rsplit(':').next().unwrap_or(after_slash);
    after_colon.trim_end_matches(".git")
}

/// Parse the first github.com remote from `jj git remote list` output,
/// returning `(remote_name, owner/repo)`.
///
/// The output format is one remote per line: `<name>\t<url>` (or
/// space-separated). Accepts both SSH (`git@github.com:owner/repo.git`)
/// and HTTPS (`https://github.com/owner/repo`) remotes. Remotes whose URL
/// is not a github.com URL — notably the local on-disk mirror that cube
/// workspaces carry — are skipped, so the returned name is always a real
/// GitHub upstream regardless of whether it is called `origin`, `github`,
/// or anything else.
pub fn parse_github_remote(remote_list_output: &str) -> Option<(String, String)> {
    for line in remote_list_output.lines() {
        // Split on the first run of whitespace to get (name, url).
        let mut iter = line.splitn(2, |c: char| c.is_whitespace());
        let name = iter.next().map(str::trim).filter(|s| !s.is_empty())?;
        if let Some(url) = iter.next().map(str::trim)
            && let Some(slug) = parse_github_slug(url)
        {
            return Some((name.to_string(), slug));
        }
    }
    None
}

/// Parsed representation of a git remote URL, normalised for equivalence checks.
#[derive(Debug, PartialEq)]
pub struct ParsedOrigin {
    /// Lower-cased host (e.g. `github.com`).
    pub host: String,
    /// Repo path without leading slash and without trailing `.git`
    /// (e.g. `linkedin-sandbox/bduff`). Case-sensitive.
    pub path: String,
}

/// Parse an SSH-style (`[user@]host:path`), `ssh://` URL, or HTTPS-style
/// (`https://[user@]host/path`) URL into a [`ParsedOrigin`]. Returns `None`
/// if the URL is not in a recognised format.
pub fn parse_origin(url: &str) -> Option<ParsedOrigin> {
    let url = url.trim();

    // HTTPS: https://[user@]host/path
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        // Drop optional `user@`
        let rest = if let Some(at) = rest.find('@') {
            &rest[at + 1..]
        } else {
            rest
        };
        let (host, path) = rest.split_once('/')?;
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        return Some(ParsedOrigin {
            host: host.to_ascii_lowercase(),
            path: path.to_string(),
        });
    }

    // RFC-3986 SSH URL: ssh://[user@]host[:port]/path
    if let Some(rest) = url.strip_prefix("ssh://") {
        // Drop optional `user@`
        let rest = if let Some(at) = rest.find('@') {
            &rest[at + 1..]
        } else {
            rest
        };
        // Drop optional `:port` from the host portion before the path `/`
        let (host_maybe_port, path) = rest.split_once('/')?;
        let host = if let Some((h, _port)) = host_maybe_port.split_once(':') {
            h
        } else {
            host_maybe_port
        };
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        return Some(ParsedOrigin {
            host: host.to_ascii_lowercase(),
            path: path.to_string(),
        });
    }

    // SSH SCP-like: [user@]host:path
    // Must contain `:` but must NOT look like a Windows absolute path (`C:\`).
    if let Some(colon_pos) = url.find(':') {
        let before_colon = &url[..colon_pos];
        let after_colon = &url[colon_pos + 1..];
        // Reject Windows paths (single letter before colon) and paths starting with `//` (git+ssh://)
        if before_colon.len() > 1 && !after_colon.starts_with('/') {
            // Strip optional `user@` from the host part
            let host = if let Some(at) = before_colon.rfind('@') {
                &before_colon[at + 1..]
            } else {
                before_colon
            };
            let path = after_colon.trim_end_matches('/').trim_end_matches(".git");
            return Some(ParsedOrigin {
                host: host.to_ascii_lowercase(),
                path: path.to_string(),
            });
        }
    }

    None
}

/// Returns `true` when two origin URL strings refer to the same repository,
/// ignoring auth-identity prefixes (e.g. `org-X@` vs `git@`) and trailing
/// `.git` suffixes. Host comparison is case-insensitive; path is case-sensitive.
pub fn origin_urls_equivalent(a: &str, b: &str) -> bool {
    match (parse_origin(a), parse_origin(b)) {
        (Some(pa), Some(pb)) => pa == pb,
        // If either URL is unparseable fall back to exact-string equality so
        // we never accidentally allow a mismatch.
        _ => a == b,
    }
}

/// Returns `true` when `origin` is a bare `owner/name` slug (e.g.
/// `linkedin-multiproduct/dev-infra`) rather than a full clone URL. Such a
/// slug has a `/` path separator but no scheme, host, or SSH `user@host:`
/// prefix, so [`parse_origin`] cannot turn it into a real origin. Boss
/// callers sometimes only carry the product's `owner/name` slug.
///
/// Contrast [`is_bare_repo_slug`], which recognises a *single* bare
/// reponame (`bduff`) carrying no `/` at all.
pub fn is_owner_name_slug(origin: &str) -> bool {
    let s = origin.trim().trim_end_matches('/');
    !s.is_empty()
        && !s.contains(':')
        && !s.contains('@')
        && !s.starts_with('/')
        && s.split('/').filter(|seg| !seg.is_empty()).count() >= 2
        && parse_origin(s).is_none()
}

/// Returns `true` when a bare `owner/name` `slug` names the same repo as the
/// registered `origin` URL — i.e. the slug equals the origin's parsed path
/// (ignoring a trailing `.git`). This compares against the *registered*
/// origin rather than synthesising one to assert with, so an "ensure" call
/// that arrives with only a slug succeeds when (and only when) it actually
/// refers to the configured repo.
pub fn origin_path_matches_slug(origin: &str, slug: &str) -> bool {
    let Some(parsed) = parse_origin(origin) else {
        return false;
    };
    let slug = slug.trim().trim_end_matches('/').trim_end_matches(".git");
    parsed.path == slug
}

/// Split a bare `<org>/<name>` slug into its two halves. Returns `None` unless
/// the input is exactly two non-empty `/`-separated segments free of URL
/// punctuation (no scheme, no `:` host separator, no `user@` prefix) — i.e. a
/// bare GitHub `owner/repo` slug rather than a clone URL or single name.
pub fn parse_org_name_shape(name: &str) -> Option<(String, String)> {
    let s = name.trim().trim_end_matches('/');
    if s.contains(':') || s.contains('@') {
        return None;
    }
    let mut parts = s.split('/');
    let org = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    if parts.next().is_some() {
        return None;
    }
    Some((org.to_string(), repo.to_string()))
}

/// True when `value` is a bare cube repo slug rather than a git remote
/// URL, scp-style remote, or filesystem path. URLs and remotes always
/// carry at least one of `:` (scheme / scp host separator), `/` (path),
/// or `@` (user); a slug like `bduff` carries none of those and no
/// whitespace.
///
/// Contrast [`is_owner_name_slug`], which recognises a bare `owner/name`
/// GitHub slug (which *does* carry a `/`).
pub fn is_bare_repo_slug(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| !matches!(c, ':' | '/' | '@') && !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── short_name_for / parse_github_owner_repo (ex boss-github) ──────────

    #[test]
    fn short_name_for_handles_ssh_and_https() {
        assert_eq!(short_name_for("git@github.com:spinyfin/mono.git"), "mono");
        assert_eq!(short_name_for("https://github.com/spinyfin/mono.git"), "mono");
        assert_eq!(short_name_for("https://github.com/foo/bar"), "bar");
    }

    #[test]
    fn parse_github_owner_repo_handles_every_shape() {
        assert_eq!(
            parse_github_owner_repo("git@github.com:spinyfin/mono.git").unwrap(),
            ("spinyfin", "mono")
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/spinyfin/mono.git").unwrap(),
            ("spinyfin", "mono")
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/spinyfin/mono").unwrap(),
            ("spinyfin", "mono")
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/spinyfin/mono/").unwrap(),
            ("spinyfin", "mono")
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/spinyfin/mono/pull/991").unwrap(),
            ("spinyfin", "mono")
        );
        assert_eq!(
            parse_github_owner_repo("  https://github.com/spinyfin/mono  ").unwrap(),
            ("spinyfin", "mono")
        );
    }

    #[test]
    fn parse_github_owner_repo_rejects_malformed() {
        assert!(matches!(
            parse_github_owner_repo("git@gitlab.com:foo/bar.git"),
            Err(ParseError::NotGithub(_))
        ));
        assert!(matches!(
            parse_github_owner_repo("https://gitlab.com/foo/bar"),
            Err(ParseError::NotGithub(_))
        ));
        assert!(matches!(
            parse_github_owner_repo("not a url"),
            Err(ParseError::NotGithub(_))
        ));
        assert!(matches!(
            parse_github_owner_repo("https://github.com/spinyfin"),
            Err(ParseError::MissingRepo(_))
        ));
    }

    #[test]
    fn parse_github_owner_repo_error_messages() {
        let err = parse_github_owner_repo("https://gitlab.com/foo/bar")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a github.com URL"), "{err}");

        let err = parse_github_owner_repo("https://github.com/spinyfin")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing repo segment"), "{err}");
    }

    // ── parse_org_name_shape (ex cube) ─────────────────────────────────────

    #[test]
    fn parse_org_name_shape_accepts_two_segments() {
        assert_eq!(
            parse_org_name_shape("spinyfin/mono"),
            Some(("spinyfin".to_string(), "mono".to_string()))
        );
    }

    #[test]
    fn parse_org_name_shape_rejects_non_slug_shapes() {
        assert_eq!(parse_org_name_shape("bduff"), None);
        assert_eq!(parse_org_name_shape("a/b/c"), None);
        assert_eq!(parse_org_name_shape("git@github.com:o/r.git"), None);
        assert_eq!(parse_org_name_shape("https://github.com/o/r"), None);
        assert_eq!(parse_org_name_shape("/mono"), None);
    }

    // ── parse_origin / origin_urls_equivalent (ex cube) ────────────────────

    #[test]
    fn parse_origin_plain_ssh() {
        let p = parse_origin("git@github.com:foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_auth_prefixed_ssh() {
        let p = parse_origin("org-132020694@github.com:linkedin-sandbox/bduff.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "linkedin-sandbox/bduff");
    }

    #[test]
    fn parse_origin_ssh_no_dot_git() {
        let p = parse_origin("git@github.com:foo/bar").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_https() {
        let p = parse_origin("https://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_https_with_user() {
        let p = parse_origin("https://myuser@github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn origin_urls_equivalent_plain_vs_auth_prefixed() {
        assert!(origin_urls_equivalent(
            "git@github.com:linkedin-sandbox/bduff.git",
            "org-132020694@github.com:linkedin-sandbox/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_auth_prefixed_vs_plain() {
        assert!(origin_urls_equivalent(
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
            "git@github.com:linkedin-sandbox/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_dot_git_vs_no_dot_git() {
        assert!(origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "git@github.com:foo/bar"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_different_path() {
        assert!(!origin_urls_equivalent(
            "git@github.com:linkedin-sandbox/bduff.git",
            "git@github.com:linkedin-eng/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_different_host() {
        assert!(!origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "git@gitlab.com:foo/bar.git"
        ));
    }

    // ── ssh:// URL form ────────────────────────────────────────────────────

    #[test]
    fn parse_origin_ssh_url_form() {
        let p = parse_origin("ssh://git@github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_ssh_url_auth_prefixed() {
        let p = parse_origin("ssh://org-132020694@github.com/linkedin-eng/ci-infra.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "linkedin-eng/ci-infra");
    }

    #[test]
    fn parse_origin_ssh_url_no_user() {
        let p = parse_origin("ssh://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_ssh_url_with_port() {
        let p = parse_origin("ssh://git@github.com:22/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn origin_urls_equivalent_ssh_url_vs_scp() {
        // ssh://git@github.com/foo/bar.git == git@github.com:foo/bar.git
        assert!(origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@github.com:foo/bar.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_scp_vs_ssh_url() {
        assert!(origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "ssh://git@github.com/foo/bar.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_ssh_url_auth_vs_scp_plain() {
        // ssh://org-X@github.com/foo/bar.git == git@github.com:foo/bar.git
        assert!(origin_urls_equivalent(
            "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git",
            "git@github.com:linkedin-eng/ci-infra.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_scp_auth_vs_ssh_url_plain() {
        assert!(origin_urls_equivalent(
            "org-132020694@github.com:linkedin-eng/ci-infra.git",
            "ssh://git@github.com/linkedin-eng/ci-infra.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_all_four_cross_products() {
        let variants = [
            "ssh://git@github.com/foo/bar.git",
            "ssh://org-132020694@github.com/foo/bar.git",
            "git@github.com:foo/bar.git",
            "org-132020694@github.com:foo/bar.git",
        ];
        for a in &variants {
            for b in &variants {
                assert!(origin_urls_equivalent(a, b), "{a} and {b} should be equivalent");
            }
        }
    }

    #[test]
    fn origin_urls_not_equivalent_ssh_url_different_path() {
        assert!(!origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@github.com:foo/baz.git"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_ssh_url_different_host() {
        assert!(!origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@gitlab.com:foo/bar.git"
        ));
    }

    // ── is_owner_name_slug / origin_path_matches_slug (ex cube) ─────────────

    #[test]
    fn is_owner_name_slug_accepts_owner_name() {
        assert!(is_owner_name_slug("linkedin-multiproduct/dev-infra"));
        assert!(is_owner_name_slug("foo/bar"));
        // Trailing slash and `.git` are tolerated.
        assert!(is_owner_name_slug("foo/bar/"));
    }

    #[test]
    fn is_owner_name_slug_rejects_real_urls_and_single_segment() {
        // Real clone URLs are not slugs.
        assert!(!is_owner_name_slug("git@github.com:foo/bar.git"));
        assert!(!is_owner_name_slug("https://github.com/foo/bar.git"));
        assert!(!is_owner_name_slug("ssh://org-1@github.com/foo/bar.git"));
        // A single bare name has no `owner/` and is not a slug.
        assert!(!is_owner_name_slug("dev-infra"));
        assert!(!is_owner_name_slug(""));
        // An absolute path is not a slug.
        assert!(!is_owner_name_slug("/foo/bar"));
    }

    #[test]
    fn origin_path_matches_slug_compares_against_registered_origin() {
        let origin = "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git";
        assert!(origin_path_matches_slug(origin, "linkedin-multiproduct/dev-infra"));
        assert!(origin_path_matches_slug(origin, "linkedin-multiproduct/dev-infra.git"));
        // Different owner does not match.
        assert!(!origin_path_matches_slug(origin, "some-other-org/dev-infra"));
        // Same name, different owner is still a mismatch.
        assert!(!origin_path_matches_slug(origin, "dev-infra"));
    }

    // ── is_bare_repo_slug — bare cube reponame (ex boss engine) ────────────

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

    // ── parse_github_slug / parse_github_remote (ex cube) ──────────────────

    #[test]
    fn parse_github_slug_handles_ssh_url() {
        assert_eq!(
            parse_github_slug("git@github.com:spinyfin/mono.git"),
            Some("spinyfin/mono".to_string()),
        );
    }

    #[test]
    fn parse_github_slug_handles_https_url() {
        assert_eq!(
            parse_github_slug("https://github.com/spinyfin/mono"),
            Some("spinyfin/mono".to_string()),
        );
    }

    #[test]
    fn parse_github_slug_handles_https_url_with_git_suffix() {
        assert_eq!(
            parse_github_slug("https://github.com/spinyfin/mono.git"),
            Some("spinyfin/mono".to_string()),
        );
    }

    #[test]
    fn parse_github_slug_returns_none_for_non_github_url() {
        assert_eq!(parse_github_slug("git@bitbucket.org:user/repo.git"), None);
    }

    #[test]
    fn parse_github_remote_returns_name_and_slug() {
        let output = "github\tgit@github.com:spinyfin/mono.git\n";
        assert_eq!(
            parse_github_remote(output),
            Some(("github".to_string(), "spinyfin/mono".to_string())),
        );
    }

    #[test]
    fn parse_github_remote_skips_local_mirror_named_origin() {
        // The cube-workspace trap: `origin` is a local on-disk mirror and the
        // real GitHub upstream is named `github`. We must select `github`,
        // never the local mirror, so pushes land on GitHub.
        let output = "\
origin\t/Users/bduff/dev/agents/repos/mono
github\tssh://org-1@github.com/spinyfin/mono.git
";
        assert_eq!(
            parse_github_remote(output),
            Some(("github".to_string(), "spinyfin/mono".to_string())),
        );
    }

    #[test]
    fn parse_github_remote_returns_none_when_only_local_mirror() {
        let output = "origin\t/Users/bduff/dev/agents/repos/mono\n";
        assert_eq!(parse_github_remote(output), None);
    }
}
