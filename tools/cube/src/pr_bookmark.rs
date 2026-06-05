/// Reserved `pr/<n>` namespace — local-only bookmarks that cube manages as
/// convenience pointers from a PR number to its head commit within a workspace.
///
/// These bookmarks are NEVER pushed to any remote. Every push path in cube
/// calls `assert_not_pr_bookmark` before executing `jj git push` to enforce
/// this invariant.

/// Returns `true` if `name` matches the reserved `pr/<digits>` pattern.
pub fn is_pr_bookmark(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("pr/") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// Returns the canonical local bookmark name for PR number `n` (e.g. `"pr/42"`).
pub fn pr_bookmark_name(n: u64) -> String {
    format!("pr/{n}")
}

/// Guard used by all push paths: returns an error if `bookmark` matches the
/// reserved `pr/<digits>` pattern, preventing a local-only bookmark from
/// accidentally leaking to a remote.
pub fn assert_not_pr_bookmark(bookmark: &str) -> Result<(), String> {
    if is_pr_bookmark(bookmark) {
        Err(format!(
            "refusing to push `{bookmark}`: the `pr/<n>` namespace is reserved for \
             local-only cube bookkeeping and must never be pushed to a remote. \
             Push the head branch instead."
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pr_bookmark_matches_valid_names() {
        assert!(is_pr_bookmark("pr/1"));
        assert!(is_pr_bookmark("pr/42"));
        assert!(is_pr_bookmark("pr/1364"));
        assert!(is_pr_bookmark("pr/99999"));
    }

    #[test]
    fn is_pr_bookmark_rejects_invalid_names() {
        assert!(!is_pr_bookmark("pr/"));
        assert!(!is_pr_bookmark("pr/abc"));
        assert!(!is_pr_bookmark("pr/1a"));
        assert!(!is_pr_bookmark("pr/1/2"));
        assert!(!is_pr_bookmark("boss/exec_abc"));
        assert!(!is_pr_bookmark("main"));
        assert!(!is_pr_bookmark(""));
        assert!(!is_pr_bookmark("pr"));
        assert!(!is_pr_bookmark("xpr/1"));
    }

    #[test]
    fn pr_bookmark_name_formats_correctly() {
        assert_eq!(pr_bookmark_name(1), "pr/1");
        assert_eq!(pr_bookmark_name(42), "pr/42");
        assert_eq!(pr_bookmark_name(1364), "pr/1364");
    }

    #[test]
    fn push_guard_rejects_pr_bookmarks() {
        let err = assert_not_pr_bookmark("pr/1364").unwrap_err();
        assert!(err.contains("pr/1364"), "error should mention the bookmark name");
        assert!(err.contains("reserved"), "error should say reserved");
    }

    #[test]
    fn push_guard_allows_normal_bookmarks() {
        assert!(assert_not_pr_bookmark("boss/exec_18b609fd9ab94d88_a7").is_ok());
        assert!(assert_not_pr_bookmark("main").is_ok());
        assert!(assert_not_pr_bookmark("my-feature").is_ok());
    }
}
