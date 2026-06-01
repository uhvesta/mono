use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct DocsLinkIntegrityCheck;

#[async_trait]
impl Check for DocsLinkIntegrityCheck {
    fn id(&self) -> &str {
        "docs-link-integrity"
    }

    fn description(&self) -> &str {
        "validates internal markdown links in changed markdown files"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for DocsLinkIntegrityCheck {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();
        let link_regex = Regex::new(r"\[[^\]]+\]\(([^)]+)\)").expect("valid markdown link regex");

        let changeset_paths: HashSet<&Path> = changeset
            .changed_files
            .iter()
            .filter(|f| !matches!(f.kind, ChangeKind::Deleted))
            .map(|f| f.path.as_path())
            .collect();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_markdown_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                continue;
            };

            for (line_index, line) in contents.lines().enumerate() {
                for captures in link_regex.captures_iter(line) {
                    let Some(matched) = captures.get(0) else {
                        continue;
                    };
                    if matched.start() > 0 && line.as_bytes()[matched.start() - 1] == b'!' {
                        continue;
                    }
                    let Some(target) = captures.get(1).map(|capture| capture.as_str().trim())
                    else {
                        continue;
                    };
                    if should_skip_link_target(target) {
                        continue;
                    }
                    if link_target_exists(&changed_file.path, target, tree, &changeset_paths) {
                        continue;
                    }

                    findings.push(Finding {
                        severity: Severity::Warning,
                        message: format!("broken internal markdown link target `{target}`"),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: Some((line_index + 1) as u32),
                            column: Some((matched.start() + 1) as u32),
                        }),
                        remediations: vec![
                            "Fix or remove the broken link target in this markdown file."
                                .to_owned(),
                        ],
                        suggested_fix: None,
                    });
                }
            }
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

fn is_markdown_file(path: &Path) -> bool {
    matches!(path.extension().and_then(|ext| ext.to_str()), Some("md"))
}

fn should_skip_link_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with('#')
}

fn link_target_exists(
    current_file: &Path,
    target: &str,
    tree: &dyn SourceTree,
    changeset_paths: &HashSet<&Path>,
) -> bool {
    let path_part = target
        .split_once('#')
        .map(|(path, _)| path)
        .unwrap_or(target)
        .trim();
    if path_part.is_empty() {
        return true;
    }

    let resolved = if path_part.starts_with('/') {
        normalize_relative_path(Path::new(path_part.trim_start_matches('/')))
    } else {
        let parent = current_file.parent().unwrap_or_else(|| Path::new(""));
        normalize_relative_path(&parent.join(path_part))
    };

    tree.exists(&resolved) || changeset_paths.contains(resolved.as_path())
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::DocsLinkIntegrityCheck;

    #[tokio::test]
    async fn flags_missing_relative_doc_link() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("create docs dir");
        fs::write(temp.path().join("docs/index.md"), "[Missing](missing.md)\n").expect("write doc");

        let check = DocsLinkIntegrityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/index.md").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn accepts_existing_relative_doc_link() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("create docs dir");
        fs::write(temp.path().join("docs/guide.md"), "guide\n").expect("write guide");
        fs::write(temp.path().join("docs/index.md"), "[Guide](guide.md)\n").expect("write doc");

        let check = DocsLinkIntegrityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/index.md").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn accepts_link_to_file_added_in_same_changeset() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs/design-docs")).expect("create docs dir");
        // The index links to new-feature.md, but that file only exists in the
        // changeset (not on disk).  This simulates adding a new doc and a link
        // to it in the same commit.
        fs::write(
            temp.path().join("docs/design-docs/index.md"),
            "[New Feature](new-feature.md)\n",
        )
        .expect("write index");

        let check = DocsLinkIntegrityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![
                    ChangedFile {
                        path: Path::new("docs/design-docs/index.md").to_path_buf(),
                        kind: ChangeKind::Modified,
                        old_path: None,
                    },
                    ChangedFile {
                        path: Path::new("docs/design-docs/new-feature.md").to_path_buf(),
                        kind: ChangeKind::Added,
                        old_path: None,
                    },
                ]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn checks_markdown_files_outside_docs_directory() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("guides")).expect("create guides dir");
        fs::write(
            temp.path().join("guides/setup.md"),
            "[Missing](missing.md)\n",
        )
        .expect("write guide");

        let check = DocsLinkIntegrityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("guides/setup.md").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }
}
