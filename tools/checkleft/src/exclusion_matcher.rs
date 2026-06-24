//! Framework-level path exclusion matcher.
//!
//! [`ExclusionMatcher`] is the single matcher core used by checkleft to decide
//! whether a repo-root-relative path is excluded from a check. It is built from
//! the union of any accumulated global exclude patterns and per-check exclude
//! patterns, both normalized to repo-root-relative coords before compilation.

use std::path::Path;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

/// A compiled glob-matcher for repo-root-relative path exclusion.
///
/// Built once per check instance from the union of:
/// - the accumulated global exclude patterns (from the `CHECKS` hierarchy)
/// - the per-check exclude patterns for that check instance
///
/// An empty matcher (no patterns) never excludes any path. Construct via
/// [`ExclusionMatcher::new`]; use [`Default`] to get the empty no-op variant.
#[derive(Debug, Clone, Default)]
pub struct ExclusionMatcher {
    inner: Option<GlobSet>,
}

impl ExclusionMatcher {
    /// Build a matcher from repo-root-relative glob patterns.
    ///
    /// Returns an error if any pattern is invalid globset syntax. An empty
    /// slice (or empty iterator) produces an empty matcher that excludes nothing.
    pub fn new(patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self::default());
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).with_context(|| format!("invalid exclude glob pattern `{pattern}`"))?;
            builder.add(glob);
        }
        let globset = builder
            .build()
            .context("failed to compile exclude patterns into a GlobSet")?;
        Ok(Self { inner: Some(globset) })
    }

    /// Returns `true` if the repo-root-relative `path` is matched by any exclude pattern.
    pub fn is_excluded(&self, path: &Path) -> bool {
        match &self.inner {
            Some(globset) => globset.is_match(path),
            None => false,
        }
    }

    /// Filter `paths` to those that are **not** excluded. Returns a `Vec` of
    /// references into the original slice.
    pub fn filter_paths<'a>(&self, paths: &'a [std::path::PathBuf]) -> Vec<&'a std::path::PathBuf> {
        paths.iter().filter(|p| !self.is_excluded(p.as_path())).collect()
    }

    /// Returns `true` if this matcher has no patterns and excludes nothing.
    pub fn is_empty(&self) -> bool {
        self.inner.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn empty_patterns_excludes_nothing() {
        let m = ExclusionMatcher::new(&[]).unwrap();
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
        assert!(!m.is_excluded(Path::new("vendor/dep/foo.rs")));
        assert!(m.is_empty());
    }

    #[test]
    fn default_excludes_nothing() {
        let m = ExclusionMatcher::default();
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
        assert!(m.is_empty());
    }

    #[test]
    fn exact_path_pattern_matches_only_that_path() {
        let m = ExclusionMatcher::new(&["Cargo.lock".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("Cargo.lock")));
        assert!(!m.is_excluded(Path::new("sub/Cargo.lock")));
        assert!(!m.is_excluded(Path::new("src/main.rs")));
    }

    #[test]
    fn double_star_glob_crosses_directories() {
        let m = ExclusionMatcher::new(&["vendor/**".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("vendor/dep/lib.rs")));
        assert!(m.is_excluded(Path::new("vendor/a/b/c/d.rs")));
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
    }

    #[test]
    fn glob_extension_wildcard() {
        let m = ExclusionMatcher::new(&["**/*.generated.ts".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("frontend/api/client.generated.ts")));
        assert!(m.is_excluded(Path::new("client.generated.ts")));
        assert!(!m.is_excluded(Path::new("frontend/api/client.ts")));
    }

    #[test]
    fn multiple_patterns_are_unioned() {
        let m = ExclusionMatcher::new(&["Cargo.lock".to_owned(), "MODULE.bazel.lock".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("Cargo.lock")));
        assert!(m.is_excluded(Path::new("MODULE.bazel.lock")));
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
    }

    #[test]
    fn subdirectory_scoped_pattern_does_not_match_sibling() {
        // backend/tests/** should exclude backend/tests/foo.rs but not tests/foo.rs
        let m = ExclusionMatcher::new(&["backend/tests/**".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("backend/tests/foo.rs")));
        assert!(!m.is_excluded(Path::new("tests/foo.rs")));
        assert!(!m.is_excluded(Path::new("other/backend/tests/foo.rs")));
    }

    #[test]
    fn filter_paths_removes_excluded_entries() {
        let m = ExclusionMatcher::new(&["**/*.lock".to_owned()]).unwrap();
        let paths = vec![
            PathBuf::from("Cargo.lock"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("MODULE.bazel.lock"),
            PathBuf::from("src/main.rs"),
        ];
        let kept: Vec<_> = m.filter_paths(&paths).into_iter().cloned().collect();
        assert_eq!(kept, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn filter_paths_on_empty_matcher_retains_all() {
        let m = ExclusionMatcher::default();
        let paths = vec![PathBuf::from("Cargo.lock"), PathBuf::from("src/lib.rs")];
        let kept: Vec<_> = m.filter_paths(&paths).into_iter().cloned().collect();
        assert_eq!(kept, paths);
    }

    #[test]
    fn invalid_glob_returns_error() {
        let result = ExclusionMatcher::new(&["[invalid".to_owned()]);
        assert!(result.is_err(), "expected error for invalid glob, got Ok");
    }
}
