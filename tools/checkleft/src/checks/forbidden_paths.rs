use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct ForbiddenPathsCheck;

impl ForbiddenPathsCheck {
    fn configure_with_dir(
        &self,
        config: &toml::Value,
        config_dir: &Path,
    ) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config, config_dir)?))
    }
}

#[async_trait]
impl Check for ForbiddenPathsCheck {
    fn id(&self) -> &str {
        "forbidden-paths"
    }

    fn description(&self) -> &str {
        "flags changed files whose paths match forbidden glob patterns"
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
impl ConfiguredCheck for CompiledForbiddenPathsConfig {
    async fn run(&self, changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            for rule in &self.rules {
                if !rule.when.contains(&changed_file.kind) {
                    continue;
                }

                let Some((matched_path, matched_pattern)) =
                    first_match(rule, changed_file, self.exclude_files.as_ref(), &self.config_dir)
                else {
                    continue;
                };

                findings.push(Finding {
                    severity: self.severity,
                    message: format!(
                        "path `{}` is forbidden for {} changes. (matched `{matched_pattern}`)",
                        matched_path.display(),
                        change_kind_name(changed_file.kind),
                    ),
                    location: Some(Location {
                        path: matched_path,
                        line: None,
                        column: None,
                    }),
                    remediation: Some(rule.remediation.clone()),
                    suggested_fix: None,
                });
            }
        }

        Ok(CheckResult {
            check_id: "forbidden-paths".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ForbiddenPathsConfig {
    #[serde(default)]
    rules: Vec<ForbiddenPathRuleConfig>,
    #[serde(default, alias = "exclude_globs")]
    exclude_files: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForbiddenPathRuleConfig {
    remediation: String,
    #[serde(default)]
    when: Vec<ChangeKind>,
    #[serde(default)]
    patterns: Vec<String>,
}

struct CompiledForbiddenPathsConfig {
    rules: Vec<CompiledForbiddenPathRule>,
    exclude_files: Option<GlobSet>,
    config_dir: PathBuf,
    severity: Severity,
}

struct CompiledForbiddenPathRule {
    remediation: String,
    when: Vec<ChangeKind>,
    pattern_strings: Vec<String>,
    patterns: GlobSet,
}

fn parse_config(config: &toml::Value, config_dir: &Path) -> Result<CompiledForbiddenPathsConfig> {
    let parsed: ForbiddenPathsConfig = config
        .clone()
        .try_into()
        .context("invalid forbidden-paths check config")?;

    if parsed.rules.is_empty() {
        bail!("forbidden-paths check config must contain at least one `rules` entry");
    }

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for (index, rule) in parsed.rules.into_iter().enumerate() {
        let field_prefix = format!("rules[{index}]");
        if rule.remediation.trim().is_empty() {
            bail!("forbidden-paths check config `{field_prefix}.remediation` must not be empty");
        }
        if rule.when.is_empty() {
            bail!(
                "forbidden-paths check config `{field_prefix}.when` must contain at least one change kind"
            );
        }
        if rule.patterns.is_empty() {
            bail!(
                "forbidden-paths check config `{field_prefix}.patterns` must contain at least one pattern"
            );
        }

        rules.push(CompiledForbiddenPathRule {
            remediation: rule.remediation,
            when: rule.when,
            pattern_strings: rule.patterns.clone(),
            patterns: compile_globset(&format!("{field_prefix}.patterns"), &rule.patterns)?,
        });
    }

    let exclude_files = if parsed.exclude_files.is_empty() {
        None
    } else {
        Some(compile_globset("exclude_files", &parsed.exclude_files)?)
    };

    Ok(CompiledForbiddenPathsConfig {
        rules,
        exclude_files,
        config_dir: config_dir.to_path_buf(),
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
    })
}

fn compile_globset(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }

    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` glob patterns"))
}

fn first_match<'a>(
    rule: &'a CompiledForbiddenPathRule,
    changed_file: &'a ChangedFile,
    exclude_files: Option<&GlobSet>,
    config_dir: &Path,
) -> Option<(PathBuf, &'a str)> {
    for candidate in candidate_paths(changed_file) {
        if exclude_files.is_some_and(|globs| is_excluded(candidate, globs, config_dir)) {
            continue;
        }

        let matches = rule.patterns.matches(candidate);
        if matches.is_empty() {
            continue;
        }

        return Some((candidate.to_path_buf(), &rule.pattern_strings[matches[0]]));
    }

    None
}

/// Returns true if `path` is within `config_dir` and matches `globs` (relative to config_dir).
/// Files outside the config_dir subtree are never excluded.
fn is_excluded(path: &Path, globs: &GlobSet, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    globs.is_match(relative)
}

fn candidate_paths(changed_file: &ChangedFile) -> Vec<&Path> {
    let mut paths = vec![changed_file.path.as_path()];
    if matches!(changed_file.kind, ChangeKind::Renamed) {
        if let Some(old_path) = changed_file.old_path.as_deref() {
            if old_path != changed_file.path.as_path() {
                paths.push(old_path);
            }
        }
    }
    paths
}

fn change_kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    use super::ForbiddenPathsCheck;

    #[tokio::test]
    async fn flags_added_path_for_matching_rule() {
        let temp = tempdir().expect("create temp dir");
        let artifact = temp.path().join("mobile/ios/.build/workspace-state.json");
        fs::create_dir_all(artifact.parent().expect("artifact parent")).expect("create dirs");
        fs::write(&artifact, "{}").expect("write artifact");

        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Generated artifacts must not be committed. Remove them from the change.", when = ["added"], patterns = ["**/.build/**"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        assert_eq!(
            result.findings[0].remediation.as_deref(),
            Some("Generated artifacts must not be committed. Remove them from the change.")
        );
        assert!(result.findings[0].message.contains("**/.build/**"));
    }

    #[tokio::test]
    async fn does_not_flag_added_file_for_modified_only_rule() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Generated artifacts must not be edited.", when = ["modified"], patterns = ["**/.build/**"] }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn flags_deleted_files_when_delete_rule_matches() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/legacy/config.toml").to_path_buf(),
                    kind: ChangeKind::Deleted,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Compatibility config must not be removed. Restore the file to the change.", when = ["deleted"], patterns = ["backend/legacy/config.toml"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0].remediation.as_deref(),
            Some("Compatibility config must not be removed. Restore the file to the change.")
        );
        assert_eq!(
            result.findings[0].location.as_ref().expect("location").path,
            Path::new("backend/legacy/config.toml")
        );
    }

    #[tokio::test]
    async fn flags_renamed_files_when_new_path_matches() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/dist/app.js").to_path_buf(),
                    kind: ChangeKind::Renamed,
                    old_path: Some(Path::new("frontend/src/app.js").to_path_buf()),
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Distribution assets must not be committed. Move them out of versioned paths.", when = ["renamed"], patterns = ["**/dist/**"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0].location.as_ref().expect("location").path,
            Path::new("frontend/dist/app.js")
        );
    }

    #[tokio::test]
    async fn flags_renamed_files_when_old_path_matches() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/app.js").to_path_buf(),
                    kind: ChangeKind::Renamed,
                    old_path: Some(Path::new("frontend/dist/app.js").to_path_buf()),
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Distribution assets must not be renamed into tracked source paths.", when = ["renamed"], patterns = ["**/dist/**"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0].location.as_ref().expect("location").path,
            Path::new("frontend/dist/app.js")
        );
    }

    #[tokio::test]
    async fn excludes_configured_paths() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Generated artifacts must not be committed.", when = ["added"], patterns = ["**/.build/**"] }]
                    exclude_files = ["mobile/ios/.build/**"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn exclude_globs_alias_still_works() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Generated artifacts must not be committed.", when = ["added"], patterns = ["**/.build/**"] }]
                    exclude_globs = ["mobile/ios/.build/**"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn exclude_files_does_not_apply_outside_config_dir_subtree() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        // Pattern "ios/.build/**" from context "mobile" should NOT exclude "other/.build/foo"
        let configured = check
            .configure_with_dir(
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "No build artifacts.", when = ["added"], patterns = ["**/.build/**"] }]
                    exclude_files = ["ios/.build/**"]
                }),
                Path::new("mobile"),
            )
            .expect("configure check");

        let result = configured
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("other/.build/foo").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
            )
            .await
            .expect("run check");

        // "other/.build/foo" is outside "mobile", so exclude_files pattern doesn't apply
        assert_eq!(result.findings.len(), 1, "file outside config_dir should not be excluded");
    }

    #[tokio::test]
    async fn emits_one_finding_per_matching_rule() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/dist/app.js.swp").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [
                        { remediation = "Distribution assets must not be committed.", when = ["added"], patterns = ["**/dist/**", "**/build/**"] },
                        { remediation = "Editor scratch files do not belong in the repo.", when = ["added", "modified"], patterns = ["**/*.swp", "**/*~"] }
                    ]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 2);
    }

    #[tokio::test]
    async fn emits_one_finding_when_multiple_patterns_match_same_rule() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/dist/app.js").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "Generated outputs must not be checked in.", when = ["added"], patterns = ["frontend/**", "**/dist/**"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn requires_at_least_one_rule() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_empty_rule_remediation() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "   ", when = ["modified"], patterns = ["backend/**"] }]
                }),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_empty_when_list() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "No edits allowed.", when = [], patterns = ["backend/**"] }]
                }),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_empty_patterns_list() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "No edits allowed.", when = ["modified"], patterns = [] }]
                }),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_glob_pattern() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ remediation = "No edits allowed.", when = ["modified"], patterns = ["["] }]
                }),
            )
            .await;

        assert!(result.is_err());
    }
}
