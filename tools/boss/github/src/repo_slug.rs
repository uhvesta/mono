//! Parse github.com remote URLs into `(owner, repo)` segments.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
