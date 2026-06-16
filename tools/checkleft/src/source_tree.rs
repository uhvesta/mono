use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};
use tracing::debug;

use crate::input::{SourceTree, TreeVersion};
use crate::path::validate_relative_path;
use crate::vcs::BaseRevision;

pub struct LocalSourceTree {
    root: PathBuf,
    base_revision: Option<BaseRevision>,
    /// VCS-tracked paths relative to `root`. When present, glob() overlays
    /// these on top of the ignore-respecting walk so that files committed
    /// before a matching .gitignore rule was added are still returned.
    tracked_paths: Option<HashSet<PathBuf>>,
}

impl LocalSourceTree {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Self::with_base_revision(root, Option::<BaseRevision>::None)
    }

    pub fn with_base_revision(
        root: impl Into<PathBuf>,
        base_revision: impl Into<Option<BaseRevision>>,
    ) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
        if !root.is_dir() {
            bail!("source tree root is not a directory: {}", root.display());
        }
        Ok(Self {
            root,
            base_revision: base_revision.into(),
            tracked_paths: None,
        })
    }

    /// Like [`with_base_revision`] but also supplies the set of VCS-tracked
    /// paths (relative to `root`) so that [`glob`] can include files that are
    /// tracked but happen to match a `.gitignore` pattern.
    pub fn with_tracked_paths(
        root: impl Into<PathBuf>,
        base_revision: impl Into<Option<BaseRevision>>,
        tracked_paths: HashSet<PathBuf>,
    ) -> Result<Self> {
        let mut tree = Self::with_base_revision(root, base_revision)?;
        tree.tracked_paths = Some(tracked_paths);
        Ok(tree)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve_checked_path(&self, relative_path: &Path) -> Result<PathBuf> {
        validate_relative_path(relative_path)?;

        let mut current = self.root.clone();
        for component in relative_path.components() {
            if let Component::Normal(part) = component {
                current.push(part);

                if let Ok(metadata) = fs::symlink_metadata(&current)
                    && metadata.file_type().is_symlink()
                {
                    let resolved = current
                        .canonicalize()
                        .with_context(|| format!("failed to resolve symlink {}", current.display()))?;
                    if !resolved.starts_with(&self.root) {
                        bail!(
                            "symlink escapes source tree root: {} -> {}",
                            current.display(),
                            resolved.display()
                        );
                    }
                }
            }
        }

        if let Ok(canonical) = current.canonicalize()
            && !canonical.starts_with(&self.root)
        {
            bail!(
                "resolved path escapes source tree root: {} -> {}",
                relative_path.display(),
                canonical.display()
            );
        }

        Ok(current)
    }

    fn path_relative_to_root(&self, path: &Path) -> Result<PathBuf> {
        path.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .with_context(|| format!("path is not under source tree root: {}", path.display()))
    }

    fn read_base_file(&self, path: &Path) -> Result<Vec<u8>> {
        validate_relative_path(path)?;
        let Some(base_revision) = self.base_revision.as_ref() else {
            bail!("base revision reads are not configured for this source tree");
        };

        match base_revision {
            BaseRevision::Git(revision) => {
                let path = path.to_string_lossy();
                let revision_arg = format!("{revision}:{path}");
                run_bytes_command(&self.root, "git", &["show", &revision_arg])
                    .with_context(|| format!("failed to read base file `{}` from git", path))
            }
            BaseRevision::Jujutsu(revision) => {
                let path = path.to_string_lossy().to_string();
                run_bytes_command(&self.root, "jj", &["file", "show", "-r", revision, &path])
                    .with_context(|| format!("failed to read base file `{}` from jj", path))
            }
        }
    }
}

impl SourceTree for LocalSourceTree {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let path = self.resolve_checked_path(path)?;
        fs::read(&path).with_context(|| format!("failed to read file {}", path.display()))
    }

    fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>> {
        match version {
            TreeVersion::Current => self.read_file(path),
            TreeVersion::Base => self.read_base_file(path),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        let Ok(path) = self.resolve_checked_path(path) else {
            return false;
        };
        fs::metadata(path).is_ok()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let directory_path = self.resolve_checked_path(path)?;
        let entries = fs::read_dir(&directory_path)
            .with_context(|| format!("failed to read directory {}", directory_path.display()))?;

        let mut output = Vec::new();
        for entry in entries {
            let entry =
                entry.with_context(|| format!("failed to read directory entry under {}", directory_path.display()))?;
            output.push(self.path_relative_to_root(&entry.path())?);
        }

        output.sort();
        Ok(output)
    }

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let candidate = Path::new(pattern);
        if candidate.is_absolute() || pattern.contains("..") {
            bail!("invalid glob pattern: {pattern}");
        }

        let mut glob_builder = GlobSetBuilder::new();
        glob_builder.add(Glob::new(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?);
        let glob_set = glob_builder.build().context("failed to build glob set")?;

        let walk_start = Instant::now();
        let mut matches: HashSet<PathBuf> = HashSet::new();
        let mut skipped_symlinks = 0usize;

        // Walk the source tree respecting .gitignore/.ignore rules so that
        // untracked ignored paths (cargo target/, node_modules/, .cache/, etc.)
        // are not materialised into every whole-repo sandbox. The ignore crate
        // reads .gitignore, .ignore, .git/info/exclude, and the global gitignore.
        for result in ignore::WalkBuilder::new(&self.root)
            .follow_links(false)
            // Include hidden files (e.g. .github/, .cargo/) — we want to check
            // them; the gitignore rules handle what should actually be excluded.
            .hidden(false)
            // Prevent descending into VCS internal directories: their contents
            // are not check inputs and can disappear mid-walk.
            .filter_entry(|e| {
                if e.file_type().is_some_and(|t| t.is_dir()) {
                    let name = e.file_name();
                    name != ".jj" && name != ".git"
                } else {
                    true
                }
            })
            .build()
        {
            let entry =
                result.with_context(|| format!("failed to walk source tree rooted at {}", self.root.display()))?;

            let Some(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                continue;
            }

            // Skip symlinks whose targets escape the source tree root or resolve
            // to a directory. Escaping symlinks are untracked artefacts (e.g.
            // `bazel-bin` in a Bazel workspace) that would cause sandbox
            // materialisation to fail. Directory symlinks (e.g. pnpm package
            // symlinks inside node_modules) cannot be read as files and must
            // not appear in a whole-repo scan.
            if ft.is_symlink() {
                let Ok(resolved) = entry.path().canonicalize() else {
                    debug!(path = %entry.path().display(), reason = "unresolvable symlink", "skipped glob entry");
                    skipped_symlinks += 1;
                    continue;
                };
                if !resolved.starts_with(&self.root) || resolved.is_dir() {
                    debug!(path = %entry.path().display(), reason = "symlink escapes tree or is dir", "skipped glob entry");
                    skipped_symlinks += 1;
                    continue;
                }
            }

            let relative_path = self.path_relative_to_root(entry.path())?;
            if glob_set.is_match(&relative_path) {
                matches.insert(relative_path);
            }
        }

        // A file that is VCS-tracked but happens to match a .gitignore rule
        // (added after the file was committed) must still be included: checks
        // operate on tracked content. The ignore walk above skipped it, so
        // overlay the tracked set: include any tracked path not already found
        // that matches the glob and still exists on the filesystem.
        if let Some(tracked) = &self.tracked_paths {
            for tracked_path in tracked {
                if !matches.contains(tracked_path) && glob_set.is_match(tracked_path) {
                    let abs_path = self.root.join(tracked_path);
                    if abs_path.is_file() {
                        matches.insert(tracked_path.clone());
                    }
                }
            }
        }

        let mut result: Vec<PathBuf> = matches.into_iter().collect();
        result.sort();
        debug!(
            pattern,
            matched = result.len(),
            skipped_symlinks,
            elapsed_ms = walk_start.elapsed().as_millis(),
            "glob walk complete"
        );
        Ok(result)
    }
}

fn run_bytes_command(root: &Path, binary: &str, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(binary)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to execute `{binary} {}`", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command `{binary} {}` failed: {}", args.join(" "), stderr.trim());
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::tempdir;

    use super::LocalSourceTree;
    use crate::input::{SourceTree, TreeVersion};
    use crate::vcs::BaseRevision;

    #[test]
    fn read_file_within_root_succeeds() {
        let temp = tempdir().expect("create temp dir");
        let file_path = temp.path().join("foo.txt");
        fs::write(&file_path, b"hello").expect("write file");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let contents = tree.read_file(Path::new("foo.txt")).expect("read file");
        assert_eq!(contents, b"hello");
    }

    #[test]
    fn read_file_rejects_escape_attempts() {
        let temp = tempdir().expect("create temp dir");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        let parent_escape = tree.read_file(Path::new("../outside.txt"));
        assert!(parent_escape.is_err());

        let absolute_escape = tree.read_file(Path::new("/tmp/outside.txt"));
        assert!(absolute_escape.is_err());
    }

    #[test]
    fn missing_paths_are_handled() {
        let temp = tempdir().expect("create temp dir");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        assert!(!tree.exists(Path::new("missing.txt")));
        assert!(tree.read_file(Path::new("missing.txt")).is_err());
    }

    #[test]
    fn glob_matches_expected_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("src/nested")).expect("create dirs");
        fs::write(temp.path().join("src/lib.rs"), "pub fn x() {}\n").expect("write file");
        fs::write(temp.path().join("src/nested/mod.rs"), "pub mod nested;\n").expect("write file");
        fs::write(temp.path().join("README.md"), "docs\n").expect("write file");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("src/**/*.rs").expect("glob files");

        assert_eq!(
            matches,
            vec![
                Path::new("src/lib.rs").to_path_buf(),
                Path::new("src/nested/mod.rs").to_path_buf()
            ]
        );
    }

    #[test]
    fn glob_excludes_jj_directory() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".jj/working_copy")).expect("create .jj dirs");
        fs::write(temp.path().join(".jj/working_copy/working_copy.lock"), b"lock").expect("write lock");
        fs::write(temp.path().join(".jj/store"), b"store data").expect("write store");
        fs::create_dir_all(temp.path().join("src")).expect("create src dir");
        fs::write(temp.path().join("src/lib.rs"), b"fn f() {}").expect("write source");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("**").expect("glob all");

        for p in &matches {
            assert!(
                !p.starts_with(".jj"),
                ".jj internal must not appear in glob results: {}",
                p.display()
            );
        }
        assert!(
            matches.contains(&Path::new("src/lib.rs").to_path_buf()),
            "source file must appear in glob results"
        );
    }

    #[test]
    fn glob_excludes_git_directory() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".git/refs/heads")).expect("create .git dirs");
        fs::write(temp.path().join(".git/HEAD"), b"ref: refs/heads/main").expect("write HEAD");
        fs::write(temp.path().join(".git/config"), b"[core]").expect("write config");
        fs::write(temp.path().join("README.md"), b"readme").expect("write readme");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("**").expect("glob all");

        for p in &matches {
            assert!(
                !p.starts_with(".git"),
                ".git internal must not appear in glob results: {}",
                p.display()
            );
        }
        assert!(
            matches.contains(&Path::new("README.md").to_path_buf()),
            "non-VCS file must appear in glob results"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_root_is_rejected() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("create temp dir");
        let outside = tempdir().expect("create outside dir");

        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, b"secret").expect("write outside file");

        let link_path = temp.path().join("link.txt");
        unix_fs::symlink(&outside_file, &link_path).expect("create symlink");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = tree.read_file(Path::new("link.txt"));
        assert!(result.is_err());
        assert!(!tree.exists(Path::new("link.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_exists_and_is_readable() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("create temp dir");
        let target = temp.path().join("target.txt");
        fs::write(&target, b"safe").expect("write target file");

        let link_path = temp.path().join("link.txt");
        unix_fs::symlink(&target, &link_path).expect("create symlink");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        assert!(tree.exists(Path::new("link.txt")));
        let content = tree.read_file(Path::new("link.txt")).expect("read through symlink");
        assert_eq!(content, b"safe");
    }

    #[test]
    fn reads_file_from_git_base_revision() {
        let temp = tempdir().expect("create temp dir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.email", "checkleft@example.com"]);
        run_git(temp.path(), &["config", "user.name", "Checkleft"]);

        fs::write(temp.path().join("tracked.txt"), "before\n").expect("write initial file");
        run_git(temp.path(), &["add", "tracked.txt"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        fs::write(temp.path().join("tracked.txt"), "after\n").expect("rewrite file");

        let tree = LocalSourceTree::with_base_revision(temp.path(), Some(BaseRevision::Git("HEAD".to_owned())))
            .expect("create tree");
        let current = tree
            .read_file_versioned(Path::new("tracked.txt"), TreeVersion::Current)
            .expect("read current file");
        let base = tree
            .read_file_versioned(Path::new("tracked.txt"), TreeVersion::Base)
            .expect("read base file");

        assert_eq!(current, b"after\n");
        assert_eq!(base, b"before\n");
    }

    /// Untracked ignored files (e.g. cargo target/) must NOT appear in glob
    /// results. Only files ignored by .gitignore AND not in the tracked set
    /// should be excluded.
    #[test]
    fn glob_excludes_gitignored_untracked_files() {
        let temp = tempdir().expect("create temp dir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_git(temp.path(), &["config", "user.name", "Test"]);

        // Commit a source file.
        fs::write(temp.path().join("src.rs"), b"fn main() {}").expect("write source");
        run_git(temp.path(), &["add", "src.rs"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        // Add a .gitignore that ignores target/.
        fs::write(temp.path().join(".gitignore"), b"/target\n").expect("write gitignore");

        // Create an untracked ignored directory (simulating cargo's target/).
        fs::create_dir_all(temp.path().join("target/debug")).expect("create target dir");
        fs::write(temp.path().join("target/debug/app"), b"binary").expect("write binary");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("**").expect("glob all");

        for p in &matches {
            assert!(
                !p.starts_with("target"),
                "ignored untracked target/ must not appear: {}",
                p.display()
            );
        }
        assert!(
            matches.contains(&Path::new("src.rs").to_path_buf()),
            "committed source file must appear"
        );
    }

    /// A file that is committed (tracked) but whose path later matches a
    /// .gitignore rule MUST still appear in glob results. Checks operate on
    /// tracked content and must not silently miss such files.
    #[test]
    fn glob_includes_tracked_but_gitignored_files() {
        let temp = tempdir().expect("create temp dir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_git(temp.path(), &["config", "user.name", "Test"]);

        // Commit a file that we will later gitignore.
        fs::write(temp.path().join("generated.rs"), b"// generated").expect("write file");
        run_git(temp.path(), &["add", "generated.rs"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        // Now add a gitignore rule that would exclude the already-committed file.
        fs::write(temp.path().join(".gitignore"), b"generated.rs\n").expect("write gitignore");

        // Build the tracked set (as the runner would) from VCS.
        let tracked_paths: HashSet<PathBuf> = vec![PathBuf::from("generated.rs")].into_iter().collect();

        let tree =
            LocalSourceTree::with_tracked_paths(temp.path(), None::<BaseRevision>, tracked_paths).expect("create tree");
        let matches = tree.glob("**").expect("glob all");

        assert!(
            matches.contains(&Path::new("generated.rs").to_path_buf()),
            "tracked-but-gitignored file must appear in glob results; got: {matches:?}"
        );
    }

    /// Without the tracked_paths overlay, a tracked-but-gitignored file is
    /// absent from glob results (this is the "pure gitignore" behavior that the
    /// task description says would over-exclude; it is the baseline to show the
    /// overlay is doing work).
    #[test]
    fn glob_excludes_gitignored_file_without_tracked_overlay() {
        let temp = tempdir().expect("create temp dir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);
        run_git(temp.path(), &["config", "user.name", "Test"]);

        fs::write(temp.path().join("generated.rs"), b"// generated").expect("write file");
        run_git(temp.path(), &["add", "generated.rs"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        fs::write(temp.path().join(".gitignore"), b"generated.rs\n").expect("write gitignore");

        // No tracked_paths overlay.
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("**").expect("glob all");

        assert!(
            !matches.contains(&Path::new("generated.rs").to_path_buf()),
            "without tracked overlay, gitignored file must not appear"
        );
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
