use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{TempDir, tempdir};

use crate::input::{ChangeSet, SourceTree};
use crate::path::validate_relative_path;

/// Declares how much of the repository a check needs to read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AccessScope {
    /// Only the files modified in the current changeset (the default).
    #[default]
    ModifiedOnly,
    /// Every file in the repository tree. Opt-in; requires explicit declaration.
    WholeRepo,
    /// Union of the declared glob patterns (repo-root-relative) plus all changeset files.
    Globs(Vec<String>),
}

/// The on-disk path of the repository root, used as the source location for
/// the hardlink optimisation when populating the sandbox.
///
/// **Contract:** `path` must equal the root of the [`SourceTree`] passed to
/// [`create_sandbox`].  All scope resolution and allowlist derivation delegates
/// to the SourceTree (which enforces its own path-containment and symlink checks);
/// the ceiling is consulted only by [`populate_sandbox_file`] when deciding
/// whether to attempt a hardlink.  Because file discovery (`glob`, `exists`) is
/// always routed through the SourceTree, the boundary property—no path outside
/// the SourceTree root enters the sandbox—is upheld by the SourceTree, not by an
/// independent intersection in the allowlist resolver.
///
/// Callers must uphold the invariant: if `ceiling.path != source_tree_root`, the
/// hardlink source will diverge from the SourceTree content and the optimisation
/// will silently serve stale data.
pub struct HostCeiling {
    path: PathBuf,
}

impl HostCeiling {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The result of creating a sandbox.
#[derive(Debug)]
pub struct SandboxResult {
    /// The populated sandbox directory. Dropping this removes the sandbox.
    pub root: TempDir,
    /// The repo-relative paths that were materialized into the sandbox, sorted.
    pub allowed_paths: Vec<PathBuf>,
}

/// Create a per-invocation filesystem sandbox for the given changeset and access scope.
///
/// Resolves the allowlist from `scope`, creates a temp directory, and populates it
/// with the allowlisted files at their repo-relative paths. Files are placed via
/// hardlink when the sandbox is on the same filesystem as the ceiling; otherwise
/// they are materialized via `source_tree.read_file`.
///
/// Path normalization and `..` traversal rejection are applied to all paths
/// (changeset and glob-derived). Any path that would escape the sandbox fails
/// the entire call. Symlink entries are always materialized via the SourceTree
/// rather than hardlinked, so the SourceTree's containment checks apply.
///
/// **Invariant:** `ceiling.path` must equal the root of `source_tree`; see
/// [`HostCeiling`] for details.
pub fn create_sandbox(
    changeset: &ChangeSet,
    scope: AccessScope,
    source_tree: &dyn SourceTree,
    ceiling: &HostCeiling,
) -> Result<SandboxResult> {
    let allowlist = resolve_allowlist(changeset, &scope, source_tree)?;

    let sandbox_root = tempdir().context("failed to create sandbox temp directory")?;

    let mut allowed_paths = Vec::with_capacity(allowlist.len());
    for path in &allowlist {
        populate_sandbox_file(sandbox_root.path(), path, &ceiling.path, source_tree)
            .with_context(|| format!("failed to populate sandbox file {}", path.display()))?;
        allowed_paths.push(path.clone());
    }

    Ok(SandboxResult {
        root: sandbox_root,
        allowed_paths,
    })
}

fn resolve_allowlist(
    changeset: &ChangeSet,
    scope: &AccessScope,
    source_tree: &dyn SourceTree,
) -> Result<Vec<PathBuf>> {
    let mut paths = match scope {
        AccessScope::ModifiedOnly => {
            let mut paths = Vec::new();
            for file in &changeset.changed_files {
                validate_relative_path(&file.path).with_context(|| {
                    format!("invalid path in changeset: {}", file.path.display())
                })?;
                if source_tree.exists(&file.path) {
                    paths.push(file.path.clone());
                }
            }
            paths
        }

        AccessScope::WholeRepo => {
            let glob_paths = source_tree
                .glob("**")
                .context("failed to enumerate whole-repo files")?;
            for p in &glob_paths {
                validate_relative_path(p).with_context(|| {
                    format!("source tree returned invalid path: {}", p.display())
                })?;
            }
            glob_paths
        }

        AccessScope::Globs(patterns) => {
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut paths: Vec<PathBuf> = Vec::new();

            // Changeset paths are always included regardless of glob patterns.
            for file in &changeset.changed_files {
                validate_relative_path(&file.path).with_context(|| {
                    format!("invalid path in changeset: {}", file.path.display())
                })?;
                if source_tree.exists(&file.path) && seen.insert(file.path.clone()) {
                    paths.push(file.path.clone());
                }
            }

            for pattern in patterns {
                let matches = source_tree
                    .glob(pattern)
                    .with_context(|| format!("failed to expand glob pattern `{pattern}`"))?;
                for p in matches {
                    validate_relative_path(&p).with_context(|| {
                        format!("source tree returned invalid path: {}", p.display())
                    })?;
                    if seen.insert(p.clone()) {
                        paths.push(p);
                    }
                }
            }

            paths
        }
    };

    // Sort all scopes for deterministic, consistent output.
    paths.sort();
    Ok(paths)
}

fn populate_sandbox_file(
    sandbox_root: &Path,
    relative_path: &Path,
    ceiling: &Path,
    source_tree: &dyn SourceTree,
) -> Result<()> {
    let dest = sandbox_root.join(relative_path);

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory {}", parent.display()))?;
    }

    // Prefer a hardlink from the ceiling (zero extra disk, fast), but only when
    // the source is a regular file.  Hard-linking a symlink copies the symlink
    // reference rather than its target, which could point outside the ceiling and
    // be followed by a check tool at runtime.  Routing symlinks through the
    // SourceTree ensures its containment checks (resolve_checked_path) are applied.
    let source = ceiling.join(relative_path);
    let source_is_symlink = fs::symlink_metadata(&source)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    if !source_is_symlink {
        if fs::hard_link(&source, &dest).is_ok() {
            return Ok(());
        }
    }

    // Fall back to materializing from the SourceTree. This handles virtual or
    // git-backed trees, cross-filesystem situations, and symlink entries (where
    // the SourceTree's path-containment checks must be applied).
    //
    // Invariant: for disk-backed trees ceiling == source_tree root, so read_file
    // and hard_link serve byte-identical content.  For virtual/git-backed trees
    // the hardlink path is never taken (ceiling files do not exist on disk), so
    // content always comes from the SourceTree and is authoritative by definition.
    let content = source_tree
        .read_file(relative_path)
        .with_context(|| format!("failed to read source file {}", relative_path.display()))?;
    fs::write(&dest, &content)
        .with_context(|| format!("failed to write sandbox file {}", dest.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{AccessScope, HostCeiling, create_sandbox};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};

    /// An in-memory SourceTree for unit tests.
    struct MapSourceTree {
        files: HashMap<PathBuf, Vec<u8>>,
    }

    impl MapSourceTree {
        fn new(entries: &[(&str, &[u8])]) -> Self {
            Self {
                files: entries
                    .iter()
                    .map(|(p, c)| (PathBuf::from(p), c.to_vec()))
                    .collect(),
            }
        }
    }

    impl SourceTree for MapSourceTree {
        fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("file not found in virtual tree: {}", path.display()))
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }

        fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
            use globset::{Glob, GlobSetBuilder};

            if Path::new(pattern).is_absolute() || pattern.contains("..") {
                anyhow::bail!("invalid glob pattern: {pattern}");
            }
            let mut builder = GlobSetBuilder::new();
            builder.add(Glob::new(pattern)?);
            let glob_set = builder.build()?;
            let mut matches: Vec<PathBuf> = self
                .files
                .keys()
                .filter(|p| glob_set.is_match(p.as_path()))
                .cloned()
                .collect();
            matches.sort();
            Ok(matches)
        }
    }

    fn changeset(paths: &[&str]) -> ChangeSet {
        ChangeSet::new(
            paths
                .iter()
                .map(|p| ChangedFile {
                    path: PathBuf::from(p),
                    kind: ChangeKind::Modified,
                    old_path: None,
                })
                .collect(),
        )
    }

    fn deleted_changeset(paths: &[&str]) -> ChangeSet {
        ChangeSet::new(
            paths
                .iter()
                .map(|p| ChangedFile {
                    path: PathBuf::from(p),
                    kind: ChangeKind::Deleted,
                    old_path: None,
                })
                .collect(),
        )
    }

    /// Create a real on-disk source tree for tests that exercise the hardlink path.
    fn disk_source_tree(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, crate::source_tree::LocalSourceTree) {
        let dir = tempdir().expect("create temp dir");
        for (path, content) in entries {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create dirs");
            }
            fs::write(&full, content).expect("write file");
        }
        let tree = crate::source_tree::LocalSourceTree::new(dir.path()).expect("create tree");
        (dir, tree)
    }

    // --- ModifiedOnly scope ---

    #[test]
    fn modified_only_includes_changed_files() {
        let tree = MapSourceTree::new(&[
            ("src/lib.rs", b"pub fn lib() {}"),
            ("src/main.rs", b"fn main() {}"),
        ]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result =
            create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert!(result.root.path().join("src/lib.rs").exists(), "lib.rs should be in sandbox");
        assert!(!result.root.path().join("src/main.rs").exists(), "main.rs should not be in sandbox");
        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn modified_only_skips_deleted_files() {
        let tree = MapSourceTree::new(&[("src/kept.rs", b"fn kept() {}")]);
        let cs = deleted_changeset(&["src/deleted.rs"]);
        let ceiling = tempdir().unwrap();
        let result =
            create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert!(result.allowed_paths.is_empty(), "deleted file should not appear in sandbox");
    }

    #[test]
    fn modified_only_empty_changeset_produces_empty_sandbox() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"pub fn lib() {}")]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();
        let result =
            create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert!(result.allowed_paths.is_empty());
    }

    // --- WholeRepo scope ---

    #[test]
    fn whole_repo_includes_all_files() {
        let tree = MapSourceTree::new(&[
            ("src/lib.rs", b"pub fn lib() {}"),
            ("src/main.rs", b"fn main() {}"),
            ("Cargo.toml", b"[package]"),
        ]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result =
            create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert!(result.root.path().join("src/lib.rs").exists());
        assert!(result.root.path().join("src/main.rs").exists());
        assert!(result.root.path().join("Cargo.toml").exists());
        assert_eq!(result.allowed_paths.len(), 3);
    }

    #[test]
    fn whole_repo_with_empty_changeset_still_enumerates_tree() {
        let tree = MapSourceTree::new(&[
            ("a.txt", b"alpha"),
            ("b.txt", b"beta"),
        ]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();
        let result =
            create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert_eq!(result.allowed_paths.len(), 2);
    }

    // --- Globs scope ---

    #[test]
    fn globs_includes_changeset_and_glob_matches() {
        let tree = MapSourceTree::new(&[
            ("src/lib.rs", b"pub fn lib() {}"),
            ("src/main.rs", b"fn main() {}"),
            ("Cargo.toml", b"[package]"),
            ("other/Cargo.toml", b"[package]"),
        ]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(
            &cs,
            AccessScope::Globs(vec!["**/Cargo.toml".to_owned()]),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        // changeset file + both Cargo.toml matches
        let paths = &result.allowed_paths;
        assert!(paths.contains(&PathBuf::from("src/lib.rs")), "changeset file must be included");
        assert!(paths.contains(&PathBuf::from("Cargo.toml")), "root Cargo.toml must be included");
        assert!(paths.contains(&PathBuf::from("other/Cargo.toml")), "nested Cargo.toml must be included");
        assert!(!paths.contains(&PathBuf::from("src/main.rs")), "non-glob non-changeset file must be excluded");
    }

    #[test]
    fn globs_changeset_files_always_included_even_with_no_patterns() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"fn f() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(
            &cs,
            AccessScope::Globs(vec![]),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn globs_no_duplicate_paths() {
        let tree = MapSourceTree::new(&[
            ("Cargo.toml", b"[package]"),
            ("src/lib.rs", b"fn f() {}"),
        ]);
        // changeset has Cargo.toml, and glob also matches it — must appear once
        let cs = changeset(&["Cargo.toml"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(
            &cs,
            AccessScope::Globs(vec!["**/Cargo.toml".to_owned()]),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert_eq!(result.allowed_paths.len(), 1);
    }

    // --- Traversal-escape rejection ---

    #[test]
    fn modified_only_rejects_parent_traversal_in_changeset() {
        let tree = MapSourceTree::new(&[]);
        let cs = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("../escape.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        let ceiling = tempdir().unwrap();
        let err = create_sandbox(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("traversal") || err.to_string().contains("invalid path"),
            "expected traversal error, got: {err}"
        );
    }

    #[test]
    fn modified_only_rejects_absolute_path_in_changeset() {
        let tree = MapSourceTree::new(&[]);
        let cs = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("/etc/passwd"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        let ceiling = tempdir().unwrap();
        let err = create_sandbox(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("absolute") || err.to_string().contains("invalid path"),
            "expected absolute path error, got: {err}"
        );
    }

    #[test]
    fn globs_rejects_parent_traversal_in_changeset() {
        let tree = MapSourceTree::new(&[]);
        let cs = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("../../outside.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        let ceiling = tempdir().unwrap();
        let err = create_sandbox(
            &cs,
            AccessScope::Globs(vec![]),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("traversal") || err.to_string().contains("invalid path"),
            "expected traversal error, got: {err}"
        );
    }

    // --- Virtual-tree materialization ---

    #[test]
    fn virtual_tree_materializes_via_source_tree_when_hardlink_unavailable() {
        // MapSourceTree has no on-disk files; hardlink from ceiling will fail.
        // The module must fall back to source_tree.read_file().
        let tree = MapSourceTree::new(&[("src/virtual.rs", b"fn virtual_fn() {}")]);
        let cs = changeset(&["src/virtual.rs"]);
        let ceiling = tempdir().unwrap(); // no files on disk here

        let result = create_sandbox(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox with virtual tree");

        let content = fs::read(result.root.path().join("src/virtual.rs"))
            .expect("read materialized file");
        assert_eq!(content, b"fn virtual_fn() {}");
        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/virtual.rs")]);
    }

    #[test]
    fn virtual_tree_whole_repo_materializes_all_files() {
        let tree = MapSourceTree::new(&[
            ("a/x.rs", b"fn x() {}"),
            ("b/y.rs", b"fn y() {}"),
        ]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();

        let result =
            create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert_eq!(
            fs::read(result.root.path().join("a/x.rs")).expect("read a/x.rs"),
            b"fn x() {}"
        );
        assert_eq!(
            fs::read(result.root.path().join("b/y.rs")).expect("read b/y.rs"),
            b"fn y() {}"
        );
    }

    // --- Hardlink optimization (real on-disk tree) ---

    #[test]
    fn hardlink_used_for_local_source_tree() {
        let (dir, tree) = disk_source_tree(&[("src/real.rs", b"fn real() {}")]);
        let cs = changeset(&["src/real.rs"]);

        let result = create_sandbox(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(dir.path()),
        )
        .expect("create sandbox with local tree");

        let content = fs::read(result.root.path().join("src/real.rs")).expect("read hardlinked file");
        assert_eq!(content, b"fn real() {}");
    }

    // --- Ordering consistency ---

    #[test]
    fn modified_only_allowed_paths_are_sorted() {
        let tree = MapSourceTree::new(&[
            ("z.rs", b"fn z() {}"),
            ("a.rs", b"fn a() {}"),
            ("m.rs", b"fn m() {}"),
        ]);
        // Provide changeset in reverse-alphabetical order.
        let cs = changeset(&["z.rs", "m.rs", "a.rs"]);
        let ceiling = tempdir().unwrap();

        let result =
            create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
                .expect("create sandbox");

        assert_eq!(
            result.allowed_paths,
            vec![
                PathBuf::from("a.rs"),
                PathBuf::from("m.rs"),
                PathBuf::from("z.rs"),
            ],
            "allowed_paths must be sorted regardless of changeset order"
        );
    }

    // --- Symlink handling ---

    #[cfg(unix)]
    #[test]
    fn symlink_pointing_outside_ceiling_is_rejected() {
        use std::os::unix::fs as unix_fs;

        // Create a disk tree with a symlink pointing outside the tree root.
        let inside = tempdir().expect("create inside dir");
        let outside = tempdir().expect("create outside dir");

        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, b"secret content").expect("write outside file");

        // Place a symlink inside the tree that points to the outside file.
        let link_path = inside.path().join("link.txt");
        unix_fs::symlink(&outside_file, &link_path).expect("create symlink");

        // Also write a regular file so the SourceTree is non-empty.
        fs::write(inside.path().join("normal.rs"), b"fn ok() {}").expect("write normal file");

        let tree = crate::source_tree::LocalSourceTree::new(inside.path())
            .expect("create tree");

        // WholeRepo would include link.txt via glob; the sandbox must reject it
        // because LocalSourceTree.read_file rejects escaping symlinks.
        let cs = ChangeSet::new(vec![]);
        let err = create_sandbox(
            &cs,
            AccessScope::WholeRepo,
            &tree,
            &HostCeiling::new(inside.path()),
        )
        .unwrap_err();

        let full_err = format!("{:#}", err);
        assert!(
            full_err.contains("symlink") || full_err.contains("escapes"),
            "expected symlink-escape error, got: {full_err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_within_ceiling_is_materialized_safely() {
        use std::os::unix::fs as unix_fs;

        // Symlink pointing to another file within the tree root — must succeed.
        let dir = tempdir().expect("create dir");
        fs::write(dir.path().join("target.rs"), b"fn target() {}").expect("write target");
        unix_fs::symlink(dir.path().join("target.rs"), dir.path().join("link.rs"))
            .expect("create symlink");

        let tree = crate::source_tree::LocalSourceTree::new(dir.path()).expect("create tree");
        let cs = ChangeSet::new(vec![]);

        let result = create_sandbox(
            &cs,
            AccessScope::WholeRepo,
            &tree,
            &HostCeiling::new(dir.path()),
        )
        .expect("sandbox creation with safe symlink must succeed");

        // Both entries are materialized; symlink is resolved to content.
        assert!(
            result.root.path().join("target.rs").exists(),
            "target.rs must be in sandbox"
        );
        assert!(
            result.root.path().join("link.rs").exists(),
            "link.rs (resolved via SourceTree) must be in sandbox"
        );
        let link_content =
            fs::read(result.root.path().join("link.rs")).expect("read link.rs content");
        assert_eq!(link_content, b"fn target() {}");
    }
}
