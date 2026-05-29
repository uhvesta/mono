use anyhow::{Context, Result};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

const DEFAULT_MAX_LINES: usize = 500;

#[derive(Debug, Default)]
pub struct FileSizeCheck;

impl FileSizeCheck {
    fn configure_with_dir(
        &self,
        config: &toml::Value,
        config_dir: &Path,
    ) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config, config_dir)?))
    }
}

#[async_trait]
impl Check for FileSizeCheck {
    fn id(&self) -> &str {
        "file-size"
    }

    fn description(&self) -> &str {
        "flags files exceeding configured line limits"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure_with_dir(config, Path::new(""))
    }

    fn configure_scoped(
        &self,
        config: &toml::Value,
        config_dir: Option<&Path>,
    ) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure_with_dir(config, config_dir.unwrap_or_else(|| Path::new("")))
    }
}

#[async_trait]
impl ConfiguredCheck for ParsedFileSizeConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if let Some(exclude_files) = &self.exclude_files {
                if is_excluded(&changed_file.path, exclude_files, &self.config_dir) {
                    continue;
                }
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };

            let line_count = contents.lines().count();
            if line_count <= self.max_lines {
                continue;
            }

            if !file_grew_in_change(changed_file, changeset) {
                continue;
            }

            let growth_message = changeset
                .file_line_deltas
                .get(&changed_file.path)
                .map(|delta| {
                    format!(
                        " File grew by +{} / -{} lines in this change.",
                        delta.added_lines, delta.removed_lines
                    )
                })
                .unwrap_or_default();

            findings.push(Finding {
                severity: Severity::Warning,
                message: format!(
                    "file has {line_count} lines, exceeding configured max_lines={}.{}",
                    self.max_lines, growth_message
                ),
                location: Some(Location {
                    path: changed_file.path.clone(),
                    line: Some((self.max_lines.saturating_add(1)) as u32),
                    column: Some(1),
                }),
                remediation: Some(
                    "Split the file or refactor into smaller modules to reduce line count."
                        .to_owned(),
                ),
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: "file-size".to_owned(),
            findings,
        })
    }
}

fn file_grew_in_change(changed_file: &crate::input::ChangedFile, changeset: &ChangeSet) -> bool {
    if matches!(changed_file.kind, ChangeKind::Added) {
        return true;
    }

    let Some(delta) = changeset.file_line_deltas.get(&changed_file.path) else {
        return false;
    };

    delta.added_lines > delta.removed_lines
}

#[derive(Debug, Deserialize)]
struct FileSizeConfig {
    #[serde(default)]
    max_lines: Option<i64>,
    #[serde(default, alias = "exclude_globs")]
    exclude_files: Option<Vec<String>>,
}

struct ParsedFileSizeConfig {
    max_lines: usize,
    exclude_files: Option<GlobSet>,
    config_dir: PathBuf,
}

fn parse_config(config: &toml::Value, config_dir: &Path) -> Result<ParsedFileSizeConfig> {
    let parsed: FileSizeConfig = config
        .clone()
        .try_into()
        .context("invalid file-size check config")?;

    let max_lines = match parsed.max_lines {
        Some(value) => {
            usize::try_from(value).context("`max_lines` must be a non-negative integer")?
        }
        None => DEFAULT_MAX_LINES,
    };

    Ok(ParsedFileSizeConfig {
        max_lines,
        exclude_files: parse_exclude_files(parsed.exclude_files.as_deref())?,
        config_dir: config_dir.to_path_buf(),
    })
}

fn parse_exclude_files(patterns: Option<&[String]>) -> Result<Option<GlobSet>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `exclude_files` pattern: {pattern}"))?;
        builder.add(glob);
    }

    let globset = builder
        .build()
        .context("failed to compile `exclude_files` patterns")?;
    Ok(Some(globset))
}

/// Returns true if `path` is within `config_dir` and matches `globs` (relative to config_dir).
/// Files outside the config_dir subtree are never excluded.
fn is_excluded(path: &Path, globs: &GlobSet, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    globs.is_match(relative)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, FileLineDelta};
    use crate::source_tree::LocalSourceTree;

    use super::FileSizeCheck;

    #[tokio::test]
    async fn flags_files_over_limit() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("big.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("big.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }])
                .with_file_line_delta(
                    Path::new("big.rs").to_path_buf(),
                    FileLineDelta {
                        added_lines: 2,
                        removed_lines: 0,
                    },
                ),
                &tree,
                &toml::Value::Table(toml::toml! { max_lines = 2 }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("max_lines=2"));
    }

    #[tokio::test]
    async fn ignores_files_within_limit() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("small.rs"), "a\nb\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("small.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! { max_lines = 5 }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_oversized_file_when_net_lines_do_not_increase() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("big.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("big.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }])
                .with_file_line_delta(
                    Path::new("big.rs").to_path_buf(),
                    FileLineDelta {
                        added_lines: 1,
                        removed_lines: 2,
                    },
                ),
                &tree,
                &toml::Value::Table(toml::toml! { max_lines = 2 }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn excludes_configured_paths() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("package-lock.json"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("package-lock.json").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    exclude_files = ["**/package-lock.json"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn exclude_globs_alias_still_works() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("package-lock.json"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("package-lock.json").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    exclude_globs = ["**/package-lock.json"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn exclude_files_does_not_apply_outside_config_dir_subtree() {
        let temp = tempdir().expect("create temp dir");
        // File lives at root level, but the check is configured from "sub/dir".
        // Pattern "oversized.rs" should only match "sub/dir/oversized.rs", NOT "oversized.rs".
        fs::write(temp.path().join("oversized.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let configured = check
            .configure_with_dir(
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    exclude_files = ["oversized.rs"]
                }),
                Path::new("sub/dir"),
            )
            .expect("configure check");

        let result = configured
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("oversized.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }])
                .with_file_line_delta(
                    Path::new("oversized.rs").to_path_buf(),
                    FileLineDelta {
                        added_lines: 2,
                        removed_lines: 0,
                    },
                ),
                &tree,
            )
            .await
            .expect("run check");

        // Pattern "oversized.rs" from sub/dir context does NOT match root-level "oversized.rs".
        assert_eq!(result.findings.len(), 1, "file outside config_dir should not be excluded");
    }

    #[tokio::test]
    async fn exclude_files_matches_within_config_dir_subtree() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("sub/dir")).expect("create dirs");
        fs::write(temp.path().join("sub/dir/oversized.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let configured = check
            .configure_with_dir(
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    exclude_files = ["oversized.rs"]
                }),
                Path::new("sub/dir"),
            )
            .expect("configure check");

        let result = configured
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("sub/dir/oversized.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }])
                .with_file_line_delta(
                    Path::new("sub/dir/oversized.rs").to_path_buf(),
                    FileLineDelta {
                        added_lines: 2,
                        removed_lines: 0,
                    },
                ),
                &tree,
            )
            .await
            .expect("run check");

        // Pattern "oversized.rs" from sub/dir context matches "sub/dir/oversized.rs".
        assert!(result.findings.is_empty(), "file inside config_dir should be excluded");
    }
}
