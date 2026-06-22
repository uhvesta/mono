use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tracing::{debug, info};

use anyhow::{Context, Result};
use rayon::prelude::*;
use tempfile::TempDir;

use crate::input::{ChangeSet, SourceTree};
use crate::path::validate_relative_path;

/// Prefix applied to every sandbox temp directory name so stale-sweep can
/// identify directories that belong to checkleft.
const SANDBOX_DIR_PREFIX: &str = "clsandbox-";

/// Stale sandbox directories older than this are swept on startup.
const STALE_SANDBOX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Return the base directory under which sandbox temp dirs should be created.
///
/// Prefers the platform cache dir (`~/Library/Caches/checkleft/sandbox` on
/// macOS, `$XDG_CACHE_HOME/checkleft/sandbox` on Linux) so that sandboxes
/// live on the same filesystem as the repo and the hardlink ceiling, enabling
/// zero-copy population via `fs::hard_link`.
///
/// Returns `None` when the cache dir cannot be determined or created (the
/// caller falls back to the system temp dir in that case).
fn sandbox_base_dir() -> Option<PathBuf> {
    let base = directories::ProjectDirs::from("", "", "checkleft").map(|p| p.cache_dir().join("sandbox"))?;
    fs::create_dir_all(&base).ok()?;
    Some(base)
}

/// Create a uniquely-named temp directory for a sandbox.
///
/// Tries the XDG / platform cache dir first (same filesystem as the repo →
/// hardlinks work). Falls back to the system temp dir when the cache dir is
/// unavailable or unwritable.
fn make_sandbox_dir(base: Option<&Path>) -> Result<TempDir> {
    if let Some(Ok(tmp)) = base.map(|dir| tempfile::Builder::new().prefix(SANDBOX_DIR_PREFIX).tempdir_in(dir)) {
        return Ok(tmp);
    }
    tempfile::Builder::new()
        .prefix(SANDBOX_DIR_PREFIX)
        .tempdir()
        .context("failed to create sandbox temp directory")
}

/// Remove sandbox directories under `base` whose mtime is older than
/// [`STALE_SANDBOX_AGE`].  Best-effort: errors are silently ignored.
fn sweep_stale_sandboxes(base: &Path) {
    let cutoff = match SystemTime::now().checked_sub(STALE_SANDBOX_AGE) {
        Some(t) => t,
        None => return,
    };
    let Ok(entries) = fs::read_dir(base) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(SANDBOX_DIR_PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Controls how a file's bytes are placed into the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyMode {
    /// Prefer a zero-copy `fs::hard_link` from the ceiling, falling back to a
    /// `SourceTree`-materialized copy. This is the default and is only safe for
    /// **read-only** consumers (the WASM check runtime): a hardlink shares the
    /// inode with the real file, so an in-place truncating write inside the
    /// sandbox would silently mutate the real file outside any copy-back.
    #[default]
    PreferHardlink,
    /// Always copy file contents; **never** hardlink. Required for any
    /// **writable** sandbox (the `fix` path): the staged file must be a distinct
    /// inode so a fixer's in-place write stays contained in the sandbox and can
    /// only reach the real tree through the controlled copy-back step.
    ForceCopy,
}

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
    /// Exactly the listed paths plus all changeset files. No globs; each entry
    /// must be a concrete repo-relative file path. Paths absent from the source
    /// tree are silently skipped. Produced by the engine after calling
    /// `declare-required-files` on a check that declares `declared-files` scope.
    ExplicitFiles(Vec<PathBuf>),
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
/// This is the read-only entry point: it uses [`CopyMode::PreferHardlink`]. A
/// caller that intends to **write** inside the sandbox (the `fix` path) must use
/// [`create_sandbox_with_mode`] with [`CopyMode::ForceCopy`] so staged files are
/// distinct inodes from the real tree.
///
/// **Invariant:** `ceiling.path` must equal the root of `source_tree`; see
/// [`HostCeiling`] for details.
pub fn create_sandbox(
    changeset: &ChangeSet,
    scope: AccessScope,
    source_tree: &dyn SourceTree,
    ceiling: &HostCeiling,
) -> Result<SandboxResult> {
    create_sandbox_with_mode(changeset, scope, source_tree, ceiling, CopyMode::PreferHardlink)
}

/// Create a sandbox with an explicit [`CopyMode`].
///
/// Identical to [`create_sandbox`] except the caller chooses whether files may be
/// hardlinked ([`CopyMode::PreferHardlink`], read-only consumers) or must always
/// be copied ([`CopyMode::ForceCopy`], writable consumers such as `fix`). Under
/// `ForceCopy` the hardlink fast path is never attempted, so every staged file is
/// a fresh inode and an in-place write inside the sandbox cannot escape to the
/// real tree.
pub fn create_sandbox_with_mode(
    changeset: &ChangeSet,
    scope: AccessScope,
    source_tree: &dyn SourceTree,
    ceiling: &HostCeiling,
    copy_mode: CopyMode,
) -> Result<SandboxResult> {
    let scope_str = match &scope {
        AccessScope::ModifiedOnly => "modified-only",
        AccessScope::WholeRepo => "whole-repo",
        AccessScope::Globs(_) => "globs",
        AccessScope::ExplicitFiles(_) => "declared-files",
    };
    let allowlist = resolve_allowlist(changeset, &scope, source_tree)?;
    debug!(scope = scope_str, files = allowlist.len(), "populating sandbox");

    let base = sandbox_base_dir();
    if let Some(ref dir) = base {
        sweep_stale_sandboxes(dir);
    }
    let sandbox_root = make_sandbox_dir(base.as_deref())?;

    // Populate files in parallel. Each file's hardlink/copy is independent, so
    // rayon's work-stealing pool (bounded by the hardware thread count) gives
    // near-linear speedup on whole-repo scopes with thousands of files.
    //
    // Correctness invariants preserved under parallelism:
    //  - Path-containment checks run inside populate_sandbox_file per file;
    //    any containment failure propagates as Err and aborts the whole build.
    //  - ENOENT (file vanished between enumeration and link/read) is skipped,
    //    not an error — same as the sequential version.
    //  - create_dir_all is idempotent; concurrent calls for the same parent
    //    directory are safe (the OS serializes the mkdir).
    //  - allowed_paths is re-sorted after collection so output is deterministic
    //    regardless of completion order.
    let populate_start = Instant::now();
    let results: Vec<Result<Option<(PathBuf, bool)>>> = allowlist
        .par_iter()
        .map(
            |path| match populate_sandbox_file(sandbox_root.path(), path, &ceiling.path, source_tree, copy_mode) {
                Ok(hardlinked) => Ok(Some((path.clone(), hardlinked))),
                Err(e) if source_file_not_found(&e) => Ok(None),
                Err(e) => Err(e).with_context(|| format!("failed to populate sandbox file {}", path.display())),
            },
        )
        .collect();

    let mut allowed_paths = Vec::with_capacity(allowlist.len());
    let mut hardlink_count = 0usize;
    let mut copy_count = 0usize;
    for result in results {
        if let Some((path, hardlinked)) = result? {
            allowed_paths.push(path);
            if hardlinked {
                hardlink_count += 1;
            } else {
                copy_count += 1;
            }
        }
    }
    // Re-sort: parallel completion order is non-deterministic.
    allowed_paths.sort();

    info!(
        scope = scope_str,
        files = allowed_paths.len(),
        hardlinks = hardlink_count,
        copies = copy_count,
        elapsed_ms = populate_start.elapsed().as_millis(),
        "sandbox populated"
    );

    Ok(SandboxResult {
        root: sandbox_root,
        allowed_paths,
    })
}

fn resolve_allowlist(changeset: &ChangeSet, scope: &AccessScope, source_tree: &dyn SourceTree) -> Result<Vec<PathBuf>> {
    let mut paths = match scope {
        AccessScope::ModifiedOnly => {
            let mut paths = Vec::new();
            for file in &changeset.changed_files {
                validate_relative_path(&file.path)
                    .with_context(|| format!("invalid path in changeset: {}", file.path.display()))?;
                if source_tree.exists(&file.path) {
                    paths.push(file.path.clone());
                }
            }
            paths
        }

        AccessScope::WholeRepo => {
            let glob_paths = source_tree.glob("**").context("failed to enumerate whole-repo files")?;
            for p in &glob_paths {
                validate_relative_path(p)
                    .with_context(|| format!("source tree returned invalid path: {}", p.display()))?;
            }
            glob_paths
        }

        AccessScope::Globs(patterns) => {
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut paths: Vec<PathBuf> = Vec::new();

            // Changeset paths are always included regardless of glob patterns.
            for file in &changeset.changed_files {
                validate_relative_path(&file.path)
                    .with_context(|| format!("invalid path in changeset: {}", file.path.display()))?;
                if source_tree.exists(&file.path) && seen.insert(file.path.clone()) {
                    paths.push(file.path.clone());
                }
            }

            for pattern in patterns {
                let matches = source_tree
                    .glob(pattern)
                    .with_context(|| format!("failed to expand glob pattern `{pattern}`"))?;
                for p in matches {
                    validate_relative_path(&p)
                        .with_context(|| format!("source tree returned invalid path: {}", p.display()))?;
                    if seen.insert(p.clone()) {
                        paths.push(p);
                    }
                }
            }

            paths
        }

        AccessScope::ExplicitFiles(extra) => {
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut paths: Vec<PathBuf> = Vec::new();

            // Changeset paths are always included.
            for file in &changeset.changed_files {
                validate_relative_path(&file.path)
                    .with_context(|| format!("invalid path in changeset: {}", file.path.display()))?;
                if source_tree.exists(&file.path) && seen.insert(file.path.clone()) {
                    paths.push(file.path.clone());
                }
            }

            // Plus every explicitly declared path that exists in the source tree.
            for p in extra {
                validate_relative_path(p).with_context(|| format!("invalid explicit path: {}", p.display()))?;
                if source_tree.exists(p) && seen.insert(p.clone()) {
                    paths.push(p.clone());
                }
            }

            paths
        }
    };

    // Sort all scopes for deterministic, consistent output.
    paths.sort();
    Ok(paths)
}

/// Returns true when `err` (or any cause in its chain) is an I/O "not found"
/// error — i.e. the source file disappeared between enumeration and the link/read.
fn source_file_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Populate one file into the sandbox. Returns `true` when the file was placed
/// via a hardlink (zero-copy, fast path) or `false` when it was materialised
/// from the SourceTree (copy path). Returns `Err` on any I/O failure other than
/// ENOENT (handled by the caller as a benign mid-walk disappearance).
///
/// Under [`CopyMode::ForceCopy`] the hardlink fast path is skipped entirely, so
/// the staged file is always a fresh inode — a write inside the sandbox cannot
/// reach the real tree.
fn populate_sandbox_file(
    sandbox_root: &Path,
    relative_path: &Path,
    ceiling: &Path,
    source_tree: &dyn SourceTree,
    copy_mode: CopyMode,
) -> Result<bool> {
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
    //
    // ForceCopy skips this block outright: a hardlink would share the real file's
    // inode, which is exactly what a writable (fix) sandbox must not do.
    if copy_mode == CopyMode::PreferHardlink {
        let source = ceiling.join(relative_path);
        let source_is_symlink = fs::symlink_metadata(&source)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);

        if !source_is_symlink && fs::hard_link(&source, &dest).is_ok() {
            debug!(path = %relative_path.display(), method = "hardlink", "populated sandbox file");
            return Ok(true);
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
    fs::write(&dest, &content).with_context(|| format!("failed to write sandbox file {}", dest.display()))?;
    debug!(path = %relative_path.display(), method = "copy", "populated sandbox file");

    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{AccessScope, CopyMode, HostCeiling, create_sandbox, create_sandbox_with_mode};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};

    /// An in-memory SourceTree for unit tests.
    struct MapSourceTree {
        files: HashMap<PathBuf, Vec<u8>>,
    }

    impl MapSourceTree {
        fn new(entries: &[(&str, &[u8])]) -> Self {
            Self {
                files: entries.iter().map(|(p, c)| (PathBuf::from(p), c.to_vec())).collect(),
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
        let tree = MapSourceTree::new(&[("src/lib.rs", b"pub fn lib() {}"), ("src/main.rs", b"fn main() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
            .expect("create sandbox");

        assert!(
            result.root.path().join("src/lib.rs").exists(),
            "lib.rs should be in sandbox"
        );
        assert!(
            !result.root.path().join("src/main.rs").exists(),
            "main.rs should not be in sandbox"
        );
        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn modified_only_skips_deleted_files() {
        let tree = MapSourceTree::new(&[("src/kept.rs", b"fn kept() {}")]);
        let cs = deleted_changeset(&["src/deleted.rs"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
            .expect("create sandbox");

        assert!(
            result.allowed_paths.is_empty(),
            "deleted file should not appear in sandbox"
        );
    }

    #[test]
    fn modified_only_empty_changeset_produces_empty_sandbox() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"pub fn lib() {}")]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
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
        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
            .expect("create sandbox");

        assert!(result.root.path().join("src/lib.rs").exists());
        assert!(result.root.path().join("src/main.rs").exists());
        assert!(result.root.path().join("Cargo.toml").exists());
        assert_eq!(result.allowed_paths.len(), 3);
    }

    #[test]
    fn whole_repo_with_empty_changeset_still_enumerates_tree() {
        let tree = MapSourceTree::new(&[("a.txt", b"alpha"), ("b.txt", b"beta")]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
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
        assert!(
            paths.contains(&PathBuf::from("src/lib.rs")),
            "changeset file must be included"
        );
        assert!(
            paths.contains(&PathBuf::from("Cargo.toml")),
            "root Cargo.toml must be included"
        );
        assert!(
            paths.contains(&PathBuf::from("other/Cargo.toml")),
            "nested Cargo.toml must be included"
        );
        assert!(
            !paths.contains(&PathBuf::from("src/main.rs")),
            "non-glob non-changeset file must be excluded"
        );
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
        let tree = MapSourceTree::new(&[("Cargo.toml", b"[package]"), ("src/lib.rs", b"fn f() {}")]);
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

    // --- ExplicitFiles scope ---

    #[test]
    fn explicit_files_includes_changeset_and_declared_paths() {
        let tree = MapSourceTree::new(&[
            ("src/lib.rs", b"fn lib() {}"),
            ("src/main.rs", b"fn main() {}"),
            ("docs/api.md", b"# API"),
            ("other.txt", b"other"),
        ]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let explicit = vec![PathBuf::from("docs/api.md"), PathBuf::from("other.txt")];
        let result = create_sandbox(
            &cs,
            AccessScope::ExplicitFiles(explicit),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert!(
            result.root.path().join("src/lib.rs").exists(),
            "changeset file must be in sandbox"
        );
        assert!(
            result.root.path().join("docs/api.md").exists(),
            "declared file must be in sandbox"
        );
        assert!(
            result.root.path().join("other.txt").exists(),
            "declared file must be in sandbox"
        );
        assert!(
            !result.root.path().join("src/main.rs").exists(),
            "undeclared file must not be in sandbox"
        );
        assert_eq!(result.allowed_paths.len(), 3);
    }

    #[test]
    fn explicit_files_skips_paths_not_in_source_tree() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"fn lib() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let explicit = vec![PathBuf::from("nonexistent.rs")];
        let result = create_sandbox(
            &cs,
            AccessScope::ExplicitFiles(explicit),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert_eq!(
            result.allowed_paths,
            vec![PathBuf::from("src/lib.rs")],
            "non-existent explicit path must be skipped"
        );
    }

    #[test]
    fn explicit_files_no_duplicates_when_declared_path_is_also_in_changeset() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"fn lib() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        // Declaring the changeset file again must not duplicate it.
        let explicit = vec![PathBuf::from("src/lib.rs")];
        let result = create_sandbox(
            &cs,
            AccessScope::ExplicitFiles(explicit),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert_eq!(
            result.allowed_paths.len(),
            1,
            "no duplicate even when explicit path overlaps changeset"
        );
    }

    #[test]
    fn explicit_files_empty_declared_list_behaves_like_modified_only() {
        let tree = MapSourceTree::new(&[("src/lib.rs", b"fn lib() {}"), ("other.rs", b"fn other() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let ceiling = tempdir().unwrap();
        let result = create_sandbox(
            &cs,
            AccessScope::ExplicitFiles(vec![]),
            &tree,
            &HostCeiling::new(ceiling.path()),
        )
        .expect("create sandbox");

        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/lib.rs")]);
        assert!(!result.root.path().join("other.rs").exists());
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
        let err = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path())).unwrap_err();
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
        let err = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path())).unwrap_err();
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

        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
            .expect("create sandbox with virtual tree");

        let content = fs::read(result.root.path().join("src/virtual.rs")).expect("read materialized file");
        assert_eq!(content, b"fn virtual_fn() {}");
        assert_eq!(result.allowed_paths, vec![PathBuf::from("src/virtual.rs")]);
    }

    #[test]
    fn virtual_tree_whole_repo_materializes_all_files() {
        let tree = MapSourceTree::new(&[("a/x.rs", b"fn x() {}"), ("b/y.rs", b"fn y() {}")]);
        let cs = ChangeSet::new(vec![]);
        let ceiling = tempdir().unwrap();

        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
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

        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(dir.path()))
            .expect("create sandbox with local tree");

        let content = fs::read(result.root.path().join("src/real.rs")).expect("read hardlinked file");
        assert_eq!(content, b"fn real() {}");
    }

    // --- Force-copy mode (writable sandbox) ---

    #[cfg(unix)]
    #[test]
    fn force_copy_does_not_hardlink_local_source_tree() {
        use std::os::unix::fs::MetadataExt;

        // PreferHardlink shares the inode; ForceCopy must produce a distinct inode
        // so a write inside the sandbox cannot escape to the real file.
        //
        // Create the source tree in the same location where sandbox_base_dir() will
        // place the sandbox (XDG cache dir or system tmp fallback). In CI (Bazel),
        // TEST_TMPDIR (used by tempfile::tempdir()) and HOME (used by
        // directories::ProjectDirs) can reside on different filesystems. If source
        // and sandbox end up on different devices, fs::hard_link fails with EXDEV
        // and PreferHardlink silently copies, making the inode assertion spurious.
        let source_parent = super::sandbox_base_dir().unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::Builder::new()
            .prefix("checkleft-test-src")
            .tempdir_in(&source_parent)
            .expect("create source temp dir");
        let src_file = dir.path().join("src/real.rs");
        fs::create_dir_all(src_file.parent().unwrap()).expect("create dirs");
        fs::write(&src_file, b"fn real() {}").expect("write source file");
        let tree = crate::source_tree::LocalSourceTree::new(dir.path()).expect("create tree");

        let cs = changeset(&["src/real.rs"]);
        let real_ino = fs::metadata(dir.path().join("src/real.rs")).unwrap().ino();

        let linked = create_sandbox_with_mode(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(dir.path()),
            CopyMode::PreferHardlink,
        )
        .expect("hardlink sandbox");
        assert_eq!(
            fs::metadata(linked.root.path().join("src/real.rs")).unwrap().ino(),
            real_ino,
            "PreferHardlink must share the inode with the real file"
        );

        let copied = create_sandbox_with_mode(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(dir.path()),
            CopyMode::ForceCopy,
        )
        .expect("force-copy sandbox");
        assert_ne!(
            fs::metadata(copied.root.path().join("src/real.rs")).unwrap().ino(),
            real_ino,
            "ForceCopy must NOT share the inode with the real file"
        );
        assert_eq!(
            fs::read(copied.root.path().join("src/real.rs")).expect("read copied file"),
            b"fn real() {}",
            "force-copied content must match the source"
        );
    }

    #[test]
    fn force_copy_write_does_not_mutate_real_file() {
        // The load-bearing safety property: rewriting the staged copy in place
        // must leave the real on-disk file byte-identical.
        let (dir, tree) = disk_source_tree(&[("a.txt", b"original")]);
        let cs = changeset(&["a.txt"]);

        let sandbox = create_sandbox_with_mode(
            &cs,
            AccessScope::ModifiedOnly,
            &tree,
            &HostCeiling::new(dir.path()),
            CopyMode::ForceCopy,
        )
        .expect("force-copy sandbox");

        // Truncating, in-place rewrite of the sandbox copy.
        fs::write(sandbox.root.path().join("a.txt"), b"REWRITTEN-IN-SANDBOX").expect("write sandbox copy");

        assert_eq!(
            fs::read(dir.path().join("a.txt")).expect("read real file"),
            b"original",
            "real file must be untouched by a write to the force-copied sandbox file"
        );
    }

    // --- Sandbox location ---

    #[test]
    fn sandbox_is_not_under_tmp_when_cache_dir_available() {
        // When the platform cache dir is available (it always is in a normal dev
        // environment), the sandbox root must NOT live under /tmp — otherwise
        // hardlinks from the repo volume fail with EXDEV on macOS.
        let (dir, tree) = disk_source_tree(&[("src/lib.rs", b"fn f() {}")]);
        let cs = changeset(&["src/lib.rs"]);
        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(dir.path()))
            .expect("create sandbox");

        if super::sandbox_base_dir().is_some() {
            let sandbox_path = result.root.path();
            let tmp_path = std::env::temp_dir();
            assert!(
                !sandbox_path.starts_with(&tmp_path),
                "sandbox {:?} must not live under system temp dir {:?} — hardlinks would fail on darwin (EXDEV)",
                sandbox_path,
                tmp_path,
            );
        }
    }

    // --- Stale sandbox sweep ---

    #[test]
    fn stale_sandbox_sweep_removes_old_dirs() {
        use std::time::{Duration, SystemTime};

        let base = tempdir().unwrap();

        // Create a "stale" sandbox dir with an old mtime.
        let stale = base.path().join("clsandbox-stale");
        fs::create_dir(&stale).unwrap();
        let old_time = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(48 * 60 * 60));
        filetime::set_file_mtime(&stale, old_time).unwrap();

        // Create a "fresh" sandbox dir with a current mtime.
        let fresh = base.path().join("clsandbox-fresh");
        fs::create_dir(&fresh).unwrap();

        // Create a non-sandbox dir — must never be touched.
        let unrelated = base.path().join("other-dir");
        fs::create_dir(&unrelated).unwrap();

        super::sweep_stale_sandboxes(base.path());

        assert!(!stale.exists(), "stale sandbox dir must be removed");
        assert!(fresh.exists(), "fresh sandbox dir must be kept");
        assert!(unrelated.exists(), "unrelated dir must not be touched");
    }

    // --- Ordering consistency ---

    #[test]
    fn modified_only_allowed_paths_are_sorted() {
        let tree = MapSourceTree::new(&[("z.rs", b"fn z() {}"), ("a.rs", b"fn a() {}"), ("m.rs", b"fn m() {}")]);
        // Provide changeset in reverse-alphabetical order.
        let cs = changeset(&["z.rs", "m.rs", "a.rs"]);
        let ceiling = tempdir().unwrap();

        let result = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &HostCeiling::new(ceiling.path()))
            .expect("create sandbox");

        assert_eq!(
            result.allowed_paths,
            vec![PathBuf::from("a.rs"), PathBuf::from("m.rs"), PathBuf::from("z.rs"),],
            "allowed_paths must be sorted regardless of changeset order"
        );
    }

    // --- VCS internals exclusion ---

    #[test]
    fn whole_repo_excludes_jj_internals() {
        let (dir, tree) = disk_source_tree(&[
            ("src/lib.rs", b"pub fn f() {}"),
            (".jj/working_copy/working_copy.lock", b"lock"),
            (".jj/store/git/config", b"[core]"),
        ]);

        let cs = ChangeSet::new(vec![]);
        let result =
            create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(dir.path())).expect("create sandbox");

        for p in &result.allowed_paths {
            assert!(
                !p.starts_with(".jj"),
                ".jj internal must not appear in sandbox: {}",
                p.display()
            );
        }
        assert!(
            result.root.path().join("src/lib.rs").exists(),
            "non-VCS file must still appear in sandbox"
        );
    }

    #[test]
    fn whole_repo_excludes_git_internals() {
        let (dir, tree) = disk_source_tree(&[
            ("README.md", b"readme"),
            (".git/config", b"[core]"),
            (".git/HEAD", b"ref: refs/heads/main"),
        ]);

        let cs = ChangeSet::new(vec![]);
        let result =
            create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(dir.path())).expect("create sandbox");

        for p in &result.allowed_paths {
            assert!(
                !p.starts_with(".git"),
                ".git internal must not appear in sandbox: {}",
                p.display()
            );
        }
        assert!(
            result.root.path().join("README.md").exists(),
            "non-VCS file must still appear in sandbox"
        );
    }

    // --- Mid-population ENOENT tolerance ---

    /// A SourceTree wrapper that simulates a file vanishing between enumeration
    /// and population: `exists` and `glob` advertise the file, but `read_file`
    /// returns NotFound for it.
    struct VanishingSourceTree {
        inner: MapSourceTree,
        vanished: PathBuf,
    }

    impl SourceTree for VanishingSourceTree {
        fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            if path == self.vanished {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
            }
            self.inner.read_file(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
            self.inner.list_dir(path)
        }

        fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
            self.inner.glob(pattern)
        }
    }

    #[test]
    fn vanished_file_does_not_abort_sandbox_population() {
        let tree = VanishingSourceTree {
            inner: MapSourceTree::new(&[("src/stable.rs", b"fn stable() {}"), ("src/volatile.rs", b"fn v() {}")]),
            vanished: PathBuf::from("src/volatile.rs"),
        };
        let ceiling = tempdir().unwrap();

        let cs = ChangeSet::new(vec![]);
        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(ceiling.path()))
            .expect("sandbox must succeed even when a file vanishes");

        assert!(
            result.root.path().join("src/stable.rs").exists(),
            "stable file must be in sandbox"
        );
        assert!(
            !result.root.path().join("src/volatile.rs").exists(),
            "vanished file must be absent"
        );
        assert!(
            result.allowed_paths.contains(&PathBuf::from("src/stable.rs")),
            "stable file must appear in allowed_paths"
        );
        assert!(
            !result.allowed_paths.contains(&PathBuf::from("src/volatile.rs")),
            "vanished file must not appear in allowed_paths"
        );
    }

    // --- Symlink handling ---

    #[cfg(unix)]
    #[test]
    fn symlink_pointing_outside_ceiling_is_silently_skipped() {
        use std::os::unix::fs as unix_fs;

        // Create a disk tree with a symlink pointing outside the tree root.
        // This simulates the `bazel-bin` / `bazel-out` symlinks that Bazel
        // creates in the workspace root — they escape the source tree and must
        // not appear in a whole-repo sandbox (they are not real repo files).
        let inside = tempdir().expect("create inside dir");
        let outside = tempdir().expect("create outside dir");

        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, b"secret content").expect("write outside file");

        // Place a symlink inside the tree that points to the outside file.
        let link_path = inside.path().join("link.txt");
        unix_fs::symlink(&outside_file, &link_path).expect("create symlink");

        // Also write a regular file so the SourceTree is non-empty.
        fs::write(inside.path().join("normal.rs"), b"fn ok() {}").expect("write normal file");

        let tree = crate::source_tree::LocalSourceTree::new(inside.path()).expect("create tree");

        // WholeRepo glob now skips symlinks that escape the tree root, so
        // sandbox creation must succeed. The sandbox contains only real repo
        // files; the escaping symlink does NOT appear in it.
        let cs = ChangeSet::new(vec![]);
        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(inside.path()))
            .expect("sandbox creation must succeed; escaping symlinks are skipped by glob");

        assert_eq!(
            result.allowed_paths,
            vec![PathBuf::from("normal.rs")],
            "only the real file must be in the sandbox; escaping symlink must be absent"
        );
        assert!(
            !result.root.path().join("link.txt").exists(),
            "escaping symlink must not be materialized into the sandbox"
        );
        assert!(
            result.root.path().join("normal.rs").exists(),
            "real file must be in the sandbox"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_within_ceiling_is_materialized_safely() {
        use std::os::unix::fs as unix_fs;

        // Symlink pointing to another file within the tree root — must succeed.
        let dir = tempdir().expect("create dir");
        fs::write(dir.path().join("target.rs"), b"fn target() {}").expect("write target");
        unix_fs::symlink(dir.path().join("target.rs"), dir.path().join("link.rs")).expect("create symlink");

        let tree = crate::source_tree::LocalSourceTree::new(dir.path()).expect("create tree");
        let cs = ChangeSet::new(vec![]);

        let result = create_sandbox(&cs, AccessScope::WholeRepo, &tree, &HostCeiling::new(dir.path()))
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
        let link_content = fs::read(result.root.path().join("link.rs")).expect("read link.rs content");
        assert_eq!(link_content, b"fn target() {}");
    }
}
