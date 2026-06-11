use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};
use walkdir::WalkDir;

use crate::input::{SourceTree, TreeVersion};
use crate::path::validate_relative_path;
use crate::vcs::BaseRevision;

pub struct LocalSourceTree {
    root: PathBuf,
    base_revision: Option<BaseRevision>,
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
        })
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

        let mut matches = Vec::new();
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry =
                entry.with_context(|| format!("failed to walk source tree rooted at {}", self.root.display()))?;

            if entry.file_type().is_dir() {
                continue;
            }

            let relative_path = self.path_relative_to_root(entry.path())?;
            if glob_set.is_match(&relative_path) {
                matches.push(relative_path);
            }
        }

        matches.sort();
        Ok(matches)
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
    use std::fs;
    use std::path::Path;
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
