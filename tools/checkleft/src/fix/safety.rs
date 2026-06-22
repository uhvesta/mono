//! Safety core: writable copy sandbox + atomic copy-back of only changed files.
//!
//! This is the load-bearing mechanism behind `checkleft fix`. Every fix —
//! declarative tool, WASM entry point, or a built-in's `suggested_fix` — funnels
//! through it, so the "touch nothing outside the fixable set" property is a
//! property of *this* code's domain rather than of any fixer's good behavior.
//!
//! The lifecycle, for a fixable set `F` (repo-relative paths):
//!
//! 1. [`WritableSandbox::stage`] copies exactly `F` into a fresh temp dir, never
//!    hardlinking ([`CopyMode::ForceCopy`]), and records a pre-fix content hash
//!    of every staged file. Force-copy is essential: a hardlink shares the real
//!    file's inode, so an in-place write inside the sandbox would escape.
//! 2. The caller runs the fixer with cwd = [`WritableSandbox::root_path`],
//!    operating on the staged copies. (Running the fixer is a later task; this
//!    module never invokes one.)
//! 3. [`WritableSandbox::detect_changes`] re-hashes every staged file and returns
//!    the changed set `C`. Because only `F` was staged and only staged paths are
//!    walked, `C ⊆ F` holds by construction; files a fixer *creates* outside `F`
//!    are never enumerated and die with the temp dir. Deletions are logged, not
//!    propagated.
//! 4. [`WritableSandbox::copy_back`] writes each `c ∈ C` back to the real tree via
//!    a same-directory temp file + atomic `rename` (mode-preserving). On the first
//!    I/O error it stops and reports exactly which files were applied — every
//!    applied file is complete, never a partial mix.
//!
//! **Failure handling.** A fixer that errors, or a crash before copy-back, leaves
//! the real tree untouched: simply drop the [`WritableSandbox`] (its [`TempDir`]
//! removes all staged work). A crash *during* copy-back leaves each file either
//! its original or its fully-fixed version, never a partial write.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tracing::{debug, warn};

use crate::external::sandbox::{AccessScope, CopyMode, HostCeiling, create_sandbox_with_mode};
use crate::input::{ChangeSet, SourceTree};
use crate::path::validate_relative_path;

/// A writable, sandboxed copy of a fixable set, with the pre-fix content hash of
/// every staged file recorded so changes can be detected after a fixer runs.
///
/// Dropping a `WritableSandbox` discards the sandbox (and any fixer output) — that
/// *is* the abort path: to leave the real tree untouched on failure, just drop it
/// without calling [`WritableSandbox::copy_back`].
#[derive(Debug)]
pub struct WritableSandbox {
    /// The populated, writable sandbox directory. Dropping it removes everything.
    root: TempDir,
    /// Pre-fix content hash of each staged file, keyed by repo-relative path.
    /// The key set is the fixable set that was actually staged — the airlock
    /// domain `F`. Paths in the requested set that were absent from the source
    /// tree are silently dropped by sandbox creation and never appear here.
    pre_hashes: BTreeMap<PathBuf, [u8; 32]>,
}

/// The outcome of an atomic copy-back pass.
#[derive(Debug, Default)]
pub struct CopyBackReport {
    /// Files atomically renamed into place, in application order. Each is a
    /// complete, valid file — never a partial write.
    pub applied: Vec<PathBuf>,
    /// If copy-back stopped early, the offending path and its error. The files in
    /// [`CopyBackReport::applied`] are still valid; nothing was half-written.
    pub failed: Option<(PathBuf, anyhow::Error)>,
}

impl CopyBackReport {
    /// True when every changed file was copied back without error.
    pub fn is_ok(&self) -> bool {
        self.failed.is_none()
    }
}

impl WritableSandbox {
    /// Stage exactly `fixable` into a fresh writable sandbox (force-copied, never
    /// hardlinked) and record each staged file's pre-fix content hash.
    ///
    /// `fixable` are repo-relative paths; any that escape the tree (`..`,
    /// absolute) are rejected. Paths absent from `source_tree` are silently
    /// skipped (they cannot be fixed) — [`WritableSandbox::staged_paths`] reports
    /// what was actually staged.
    ///
    /// `ceiling` must equal the root of `source_tree` (see [`HostCeiling`]).
    pub fn stage(fixable: &[PathBuf], source_tree: &dyn SourceTree, ceiling: &HostCeiling) -> Result<Self> {
        for path in fixable {
            validate_relative_path(path).with_context(|| format!("invalid fixable path: {}", path.display()))?;
        }

        // Stage EXACTLY the fixable set. An empty changeset means the
        // `ExplicitFiles` scope contributes nothing beyond `fixable`, and
        // `ForceCopy` guarantees every staged file is a distinct inode.
        let empty = ChangeSet::new(Vec::new());
        let sandbox = create_sandbox_with_mode(
            &empty,
            AccessScope::ExplicitFiles(fixable.to_vec()),
            source_tree,
            ceiling,
            CopyMode::ForceCopy,
        )
        .context("failed to stage writable fix sandbox")?;

        // `allowed_paths` is the authoritative staged set: the subset of `fixable`
        // that exists in the source tree. Hash each staged copy as it sits now —
        // these bytes are the pre-fix baseline.
        let mut pre_hashes = BTreeMap::new();
        for path in &sandbox.allowed_paths {
            let staged = sandbox.root.path().join(path);
            let bytes = fs::read(&staged)
                .with_context(|| format!("failed to read staged sandbox file {}", staged.display()))?;
            pre_hashes.insert(path.clone(), hash_bytes(&bytes));
        }

        debug!(staged = pre_hashes.len(), "staged writable fix sandbox");
        Ok(Self {
            root: sandbox.root,
            pre_hashes,
        })
    }

    /// The sandbox root. A fixer runs with this as its working directory and
    /// operates on the staged copies under it.
    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    /// The repo-relative paths that were actually staged (the fixable set `F`),
    /// sorted.
    pub fn staged_paths(&self) -> Vec<PathBuf> {
        self.pre_hashes.keys().cloned().collect()
    }

    /// Re-hash every staged file and return the changed set `C` (sorted).
    ///
    /// A staged file the fixer *deleted* is logged and omitted — deletions are
    /// never propagated to the real tree. Files created outside `F` are not
    /// staged paths, so they are never inspected (the airlock holds structurally).
    pub fn detect_changes(&self) -> Result<Vec<PathBuf>> {
        let mut changed = Vec::new();
        for (path, pre_hash) in &self.pre_hashes {
            let staged = self.root.path().join(path);
            match fs::read(&staged) {
                Ok(bytes) => {
                    if &hash_bytes(&bytes) != pre_hash {
                        changed.push(path.clone());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    warn!(path = %path.display(), "fixer deleted a staged file; deletion not propagated");
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to re-read staged file {}", staged.display()));
                }
            }
        }
        // `pre_hashes` iterates in sorted order, so `changed` is already sorted;
        // sort again defensively to make the contract explicit.
        changed.sort();
        Ok(changed)
    }

    /// Atomically copy each path in `changed` from the sandbox back to
    /// `dest_root` (the real working tree root).
    ///
    /// Files are applied in a deterministic sorted order. Each write goes to a
    /// same-directory temp file and is renamed over the target (atomic per file,
    /// mode-preserving). On the **first** I/O error, copy-back stops and the
    /// returned [`CopyBackReport`] names the files already applied plus the
    /// failure — the applied files are complete, and the failing target is
    /// untouched.
    ///
    /// **Airlock:** any path not in the staged set `F` is refused before a single
    /// byte is written. [`WritableSandbox::detect_changes`] only ever yields
    /// staged paths, so this can only trip on misuse — it is enforced defensively
    /// rather than trusted.
    pub fn copy_back(&self, changed: &[PathBuf], dest_root: &Path) -> CopyBackReport {
        let mut report = CopyBackReport::default();

        let mut ordered: Vec<&PathBuf> = changed.iter().collect();
        ordered.sort();
        ordered.dedup();

        for path in ordered {
            if !self.pre_hashes.contains_key(path.as_path()) {
                report.failed = Some((
                    path.clone(),
                    anyhow!("airlock violation: {} is not in the staged fixable set", path.display()),
                ));
                return report;
            }

            if let Err(e) = self.copy_back_one(path, dest_root) {
                report.failed = Some((path.clone(), e));
                return report; // first-error-stop: never half-write across files
            }
            report.applied.push(path.clone());
        }

        report
    }

    /// Copy one staged file back to the real tree via a same-directory temp file
    /// and an atomic rename, preserving the target's mode.
    fn copy_back_one(&self, path: &Path, dest_root: &Path) -> Result<()> {
        let src = self.root.path().join(path);
        let dest = dest_root.join(path);
        let parent = dest
            .parent()
            .ok_or_else(|| anyhow!("copy-back target has no parent directory: {}", dest.display()))?;

        let fixed = fs::read(&src).with_context(|| format!("failed to read fixed sandbox file {}", src.display()))?;

        // A temp file in the SAME directory as the target lives on the same
        // filesystem, so the final `rename` is atomic and never EXDEV.
        let mut tmp = tempfile::Builder::new()
            .prefix(".clfix-")
            .tempfile_in(parent)
            .with_context(|| format!("failed to create copy-back temp file in {}", parent.display()))?;
        tmp.write_all(&fixed)
            .with_context(|| format!("failed to write copy-back temp file for {}", dest.display()))?;
        tmp.as_file()
            .sync_all()
            .with_context(|| format!("failed to flush copy-back temp file for {}", dest.display()))?;

        // Preserve the target's mode (e.g. the executable bit). The real file is
        // still untouched here, so its current mode is the original mode.
        preserve_mode(&dest, tmp.as_file())?;

        tmp.persist(&dest)
            .map_err(|e| anyhow!(e.error))
            .with_context(|| format!("failed to atomically rename fixed file into place: {}", dest.display()))?;
        debug!(path = %path.display(), "copied fixed file back to real tree");
        Ok(())
    }
}

/// SHA-256 of `bytes` as a fixed-size array, for cheap content-equality checks.
fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Apply the target file's current mode to the copy-back temp file so the rename
/// preserves permissions (the executable bit, in particular). When the target's
/// mode cannot be read, fall back to a conventional `0o644` rather than leaking
/// the temp file's restrictive `0o600`.
#[cfg(unix)]
fn preserve_mode(dest: &Path, tmp: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(dest).map(|m| m.permissions().mode()).unwrap_or(0o644);
    tmp.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to preserve mode {mode:o} for {}", dest.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn preserve_mode(_dest: &Path, _tmp: &fs::File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::{TempDir, tempdir};

    use super::WritableSandbox;
    use crate::external::sandbox::HostCeiling;
    use crate::source_tree::LocalSourceTree;

    /// Build a real on-disk tree and a `LocalSourceTree` over it. The same dir
    /// doubles as the copy-back destination root, mirroring production where the
    /// ceiling, the source tree, and the real working tree are one directory.
    fn disk_tree(entries: &[(&str, &[u8])]) -> (TempDir, LocalSourceTree) {
        let dir = tempdir().expect("temp dir");
        for (path, content) in entries {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create dirs");
            }
            fs::write(&full, content).expect("write file");
        }
        let tree = LocalSourceTree::new(dir.path()).expect("create tree");
        (dir, tree)
    }

    fn paths(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    /// Rewrite a staged file in the sandbox, simulating a fixer's in-place edit.
    fn rewrite_staged(sandbox: &WritableSandbox, rel: &str, content: &[u8]) {
        fs::write(sandbox.root_path().join(rel), content).expect("rewrite staged file");
    }

    #[test]
    fn stage_copies_exactly_the_fixable_set() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);
        let sandbox =
            WritableSandbox::stage(&paths(&["a.txt", "b.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");

        assert_eq!(sandbox.staged_paths(), paths(&["a.txt", "b.txt"]));
        assert!(sandbox.root_path().join("a.txt").exists());
        assert!(sandbox.root_path().join("b.txt").exists());
        assert!(
            !sandbox.root_path().join("c.txt").exists(),
            "unstaged file must not be in the sandbox"
        );
        assert_eq!(fs::read(sandbox.root_path().join("a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn stage_skips_paths_absent_from_tree() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let sandbox = WritableSandbox::stage(&paths(&["a.txt", "ghost.txt"]), &tree, &HostCeiling::new(dir.path()))
            .expect("stage");

        assert_eq!(
            sandbox.staged_paths(),
            paths(&["a.txt"]),
            "a path absent from the tree cannot be fixed and must be dropped"
        );
    }

    #[test]
    fn stage_rejects_traversal_path() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let err = WritableSandbox::stage(&paths(&["../escape.txt"]), &tree, &HostCeiling::new(dir.path())).unwrap_err();
        assert!(
            err.to_string().contains("invalid fixable path") || err.to_string().contains("traversal"),
            "expected a traversal rejection, got: {err}"
        );
    }

    #[test]
    fn detect_changes_reports_only_modified_files() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);
        let sandbox = WritableSandbox::stage(
            &paths(&["a.txt", "b.txt", "c.txt"]),
            &tree,
            &HostCeiling::new(dir.path()),
        )
        .expect("stage");

        rewrite_staged(&sandbox, "a.txt", b"AAA");
        rewrite_staged(&sandbox, "c.txt", b"ccc"); // rewritten with identical bytes → unchanged

        assert_eq!(
            sandbox.detect_changes().expect("detect"),
            paths(&["a.txt"]),
            "only the byte-different file is changed"
        );
    }

    #[test]
    fn no_changes_means_empty_copy_back() {
        // Idempotency: a fixer that does nothing yields no writes.
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");

        let changed = sandbox.detect_changes().expect("detect");
        assert!(changed.is_empty());

        let report = sandbox.copy_back(&changed, dir.path());
        assert!(report.is_ok());
        assert!(report.applied.is_empty());
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"aaa",
            "real file untouched"
        );
    }

    #[test]
    fn copy_back_writes_only_changed_files_to_real_tree() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);
        let sandbox = WritableSandbox::stage(
            &paths(&["a.txt", "b.txt", "c.txt"]),
            &tree,
            &HostCeiling::new(dir.path()),
        )
        .expect("stage");

        rewrite_staged(&sandbox, "a.txt", b"AAA");

        let changed = sandbox.detect_changes().expect("detect");
        let report = sandbox.copy_back(&changed, dir.path());

        assert!(report.is_ok());
        assert_eq!(report.applied, paths(&["a.txt"]));
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"AAA",
            "changed file written"
        );
        assert_eq!(
            fs::read(dir.path().join("b.txt")).unwrap(),
            b"bbb",
            "unchanged file left alone"
        );
        assert_eq!(
            fs::read(dir.path().join("c.txt")).unwrap(),
            b"ccc",
            "unchanged file left alone"
        );
    }

    #[test]
    fn dropping_sandbox_without_copy_back_leaves_tree_untouched() {
        // The abort path: a fixer errored, so the caller drops the sandbox. No
        // copy-back happened, so the real tree is byte-identical.
        let (dir, tree) = disk_tree(&[("a.txt", b"original")]);
        let sandbox_root_path;
        {
            let sandbox =
                WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");
            sandbox_root_path = sandbox.root_path().to_path_buf();
            rewrite_staged(&sandbox, "a.txt", b"FIXED-BUT-ABORTED");
            // Drop without copy_back.
        }
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"original",
            "real file must be unchanged when copy-back never ran"
        );
        assert!(!sandbox_root_path.exists(), "sandbox dir must be discarded on drop");
    }

    #[test]
    fn deleted_staged_file_is_not_propagated() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");

        fs::remove_file(sandbox.root_path().join("a.txt")).expect("delete staged file");

        let changed = sandbox.detect_changes().expect("detect");
        assert!(
            changed.is_empty(),
            "a deleted staged file must not appear in the changed set"
        );

        let report = sandbox.copy_back(&changed, dir.path());
        assert!(report.is_ok());
        assert!(
            dir.path().join("a.txt").exists(),
            "real file must NOT be deleted; deletions are never propagated"
        );
    }

    #[test]
    fn copy_back_refuses_path_outside_fixable_set() {
        // Airlock: even if a caller hands copy_back a path that was never staged,
        // it must refuse to write it.
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa"), ("b.txt", b"bbb")]);
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");

        let report = sandbox.copy_back(&paths(&["b.txt"]), dir.path());

        assert!(report.applied.is_empty());
        let (path, err) = report.failed.expect("airlock must reject the write");
        assert_eq!(path, PathBuf::from("b.txt"));
        assert!(err.to_string().contains("airlock"), "got: {err}");
        assert_eq!(
            fs::read(dir.path().join("b.txt")).unwrap(),
            b"bbb",
            "the non-fixable file must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_back_stops_at_first_error_without_half_writing() {
        use std::os::unix::fs::PermissionsExt;

        // Two changed files in two different parent dirs. The second dir is made
        // read-only so its copy-back fails; the first must already be applied and
        // complete, and the failing target must be untouched.
        let (dir, tree) = disk_tree(&[("x/a.txt", b"aaa"), ("y/b.txt", b"bbb")]);
        let sandbox = WritableSandbox::stage(&paths(&["x/a.txt", "y/b.txt"]), &tree, &HostCeiling::new(dir.path()))
            .expect("stage");

        rewrite_staged(&sandbox, "x/a.txt", b"AAA");
        rewrite_staged(&sandbox, "y/b.txt", b"BBB");

        // Make dir `y` unwritable so creating the copy-back temp file there fails.
        let y = dir.path().join("y");
        fs::set_permissions(&y, fs::Permissions::from_mode(0o555)).expect("chmod y read-only");

        let report = sandbox.copy_back(&paths(&["x/a.txt", "y/b.txt"]), dir.path());

        // Restore perms so the TempDir can clean itself up.
        fs::set_permissions(&y, fs::Permissions::from_mode(0o755)).expect("restore y perms");

        assert_eq!(
            report.applied,
            paths(&["x/a.txt"]),
            "first file applied before the error"
        );
        let (failed_path, _) = report.failed.expect("second file must fail");
        assert_eq!(failed_path, PathBuf::from("y/b.txt"));

        assert_eq!(
            fs::read(dir.path().join("x/a.txt")).unwrap(),
            b"AAA",
            "the applied file is complete, not partial"
        );
        assert_eq!(
            fs::read(dir.path().join("y/b.txt")).unwrap(),
            b"bbb",
            "the failing target is untouched (its original bytes)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_back_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, tree) = disk_tree(&[("run.sh", b"#!/bin/sh\necho hi\n")]);
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).expect("chmod 0755");

        let sandbox = WritableSandbox::stage(&paths(&["run.sh"]), &tree, &HostCeiling::new(dir.path())).expect("stage");
        rewrite_staged(&sandbox, "run.sh", b"#!/bin/sh\necho fixed\n");

        let changed = sandbox.detect_changes().expect("detect");
        let report = sandbox.copy_back(&changed, dir.path());
        assert!(report.is_ok());

        let mode = fs::metadata(dir.path().join("run.sh")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable bit must be preserved across copy-back");
        assert_eq!(
            fs::read(dir.path().join("run.sh")).unwrap(),
            b"#!/bin/sh\necho fixed\n",
            "content must be the fixed bytes"
        );
    }

    #[test]
    fn copy_back_is_atomic_per_file_distinct_inode() {
        // The renamed file is a fresh inode, proving an atomic replace rather than
        // an in-place truncate-and-write of the original.
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        #[cfg(unix)]
        let original_ino = fs::metadata(dir.path().join("a.txt")).unwrap().ino();

        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");
        rewrite_staged(&sandbox, "a.txt", b"AAA");

        let changed = sandbox.detect_changes().expect("detect");
        let report = sandbox.copy_back(&changed, dir.path());
        assert!(report.is_ok());

        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"AAA");
        #[cfg(unix)]
        assert_ne!(
            fs::metadata(dir.path().join("a.txt")).unwrap().ino(),
            original_ino,
            "atomic rename replaces the file, so the inode changes"
        );
    }

    #[test]
    fn empty_fixable_set_is_a_no_op() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let sandbox = WritableSandbox::stage(&[], &tree, &HostCeiling::new(dir.path())).expect("stage");

        assert!(sandbox.staged_paths().is_empty());
        let changed = sandbox.detect_changes().expect("detect");
        assert!(changed.is_empty());
        let report = sandbox.copy_back(&changed, dir.path());
        assert!(report.is_ok());
        assert!(report.applied.is_empty());
    }

    #[test]
    fn copy_back_orders_files_deterministically() {
        let (dir, tree) = disk_tree(&[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]);
        let sandbox = WritableSandbox::stage(
            &paths(&["a.txt", "b.txt", "c.txt"]),
            &tree,
            &HostCeiling::new(dir.path()),
        )
        .expect("stage");
        rewrite_staged(&sandbox, "a.txt", b"A");
        rewrite_staged(&sandbox, "b.txt", b"B");
        rewrite_staged(&sandbox, "c.txt", b"C");

        // Hand copy_back the changed set in scrambled order; applied order is sorted.
        let report = sandbox.copy_back(&paths(&["c.txt", "a.txt", "b.txt"]), dir.path());
        assert!(report.is_ok());
        assert_eq!(report.applied, paths(&["a.txt", "b.txt", "c.txt"]));
    }

    /// `dest_root` need not be the source tree: copy-back targets whatever root it
    /// is handed, so the staged fix can be applied to the real working tree even
    /// when staging read from a different (e.g. virtual) source.
    #[test]
    fn copy_back_targets_the_given_dest_root() {
        let (dir, tree) = disk_tree(&[("a.txt", b"aaa")]);
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");
        rewrite_staged(&sandbox, "a.txt", b"AAA");

        let dest = tempdir().expect("dest dir");
        fs::write(dest.path().join("a.txt"), b"old-dest").expect("seed dest file");

        let changed = sandbox.detect_changes().expect("detect");
        let report = sandbox.copy_back(&changed, dest.path());
        assert!(report.is_ok());

        assert_eq!(
            fs::read(dest.path().join("a.txt")).unwrap(),
            b"AAA",
            "dest_root is written"
        );
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"aaa",
            "the source tree is not the copy-back target and stays untouched"
        );
    }
}
