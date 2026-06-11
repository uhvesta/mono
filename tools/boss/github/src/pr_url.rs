//! Parsing for canonical GitHub PR URLs.
//!
//! Every PR URL the engine handles has the canonical shape
//! `https://github.com/<owner>/<repo>/pull/<N>`. Several modules used to
//! carry their own near-duplicate parsers — some naive (a bare
//! last-segment `parse()`), some robust (validating the `https://github.com/`
//! prefix and the `pull` path segment). This module is the single home for
//! that logic so every call site gets the same stricter validation.

/// Parse `(owner, repo, number)` from a canonical GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
///
/// Returns `None` for any URL that doesn't match the canonical shape: a
/// missing/wrong host prefix, an empty owner or repo, a third segment that
/// isn't `pull`, or a trailing segment that isn't a number.
pub fn parse_pr_url_parts(pr_url: &str) -> Option<(&str, &str, u64)> {
    let path = pr_url.strip_prefix("https://github.com/")?;
    let mut parts = path.splitn(4, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    let number: u64 = parts.next()?.parse().ok()?;
    Some((owner, repo, number))
}

/// Extract `"owner/repo"` from a canonical GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
pub fn repo_from_pr_url(pr_url: &str) -> Option<&str> {
    let (owner, repo, _) = parse_pr_url_parts(pr_url)?;
    let path = pr_url.strip_prefix("https://github.com/")?;
    let end = owner.len() + 1 + repo.len();
    Some(&path[..end])
}

/// Extract the PR number from a canonical GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
pub fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    parse_pr_url_parts(pr_url).map(|(_, _, number)| number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_url_parts_extracts_all_fields() {
        assert_eq!(
            parse_pr_url_parts("https://github.com/spinyfin/mono/pull/568"),
            Some(("spinyfin", "mono", 568)),
        );
        assert_eq!(
            parse_pr_url_parts("https://github.com/owner/my-repo/pull/1"),
            Some(("owner", "my-repo", 1)),
        );
    }

    #[test]
    fn parse_pr_url_parts_rejects_non_canonical() {
        // Wrong host.
        assert_eq!(parse_pr_url_parts("https://example.com/owner/repo/pull/1"), None);
        // Not a URL at all.
        assert_eq!(parse_pr_url_parts("not-a-url"), None);
        // Missing the `pull` segment.
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/issues/1"), None);
        // Trailing segment isn't a number.
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/pull/abc"), None);
        // Empty owner.
        assert_eq!(parse_pr_url_parts("https://github.com//repo/pull/1"), None);
        // Bare domain with nothing after it.
        assert_eq!(parse_pr_url_parts("https://github.com/"), None);
    }

    #[test]
    fn repo_from_pr_url_extracts_owner_repo() {
        assert_eq!(
            repo_from_pr_url("https://github.com/spinyfin/mono/pull/568"),
            Some("spinyfin/mono"),
        );
        assert_eq!(
            repo_from_pr_url("https://github.com/owner/my-repo/pull/1"),
            Some("owner/my-repo"),
        );
        assert_eq!(repo_from_pr_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(repo_from_pr_url("not-a-url"), None);
    }

    #[test]
    fn pr_number_from_url_extracts_number() {
        assert_eq!(
            pr_number_from_url("https://github.com/spinyfin/mono/pull/568"),
            Some(568),
        );
        assert_eq!(pr_number_from_url("https://github.com/owner/my-repo/pull/1"), Some(1),);
        assert_eq!(pr_number_from_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(pr_number_from_url("not-a-url"), None);
    }
}
