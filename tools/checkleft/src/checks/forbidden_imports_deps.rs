use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct ForbiddenImportsDepsCheck;

impl ForbiddenImportsDepsCheck {
    fn configure_with_dir(&self, config: &toml::Value, config_dir: &Path) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config, config_dir)?))
    }
}

#[async_trait]
impl Check for ForbiddenImportsDepsCheck {
    fn id(&self) -> &str {
        "forbidden-imports-deps"
    }

    fn description(&self) -> &str {
        "flags changed files containing forbidden dependency/import patterns"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure_with_dir(config, Path::new(""))
    }

    fn configure_scoped(&self, config: &toml::Value, config_dir: Option<&Path>) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure_with_dir(config, config_dir.unwrap_or_else(|| Path::new("")))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledForbiddenImportsDepsConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                continue;
            };

            for (line_index, line) in contents.lines().enumerate() {
                for rule in &self.rules {
                    if !rule.applies_to(&changed_file.path) {
                        continue;
                    }
                    if !rule.pattern.is_match(line) {
                        continue;
                    }

                    findings.push(Finding {
                        severity: rule.severity,
                        message: rule.message.clone(),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: Some((line_index + 1) as u32),
                            column: Some(1),
                        }),
                        remediations: vec![rule.remediation.clone()],
                        suggested_fix: None,
                    });
                }
            }
        }

        Ok(CheckResult {
            check_id: "forbidden-imports-deps".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ForbiddenImportsDepsConfig {
    #[serde(default)]
    rules: Vec<ForbiddenImportsDepsRuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForbiddenImportsDepsRuleConfig {
    pattern: String,
    message: String,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default, alias = "exclude_globs")]
    exclude_files: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledForbiddenImportsDepsConfig {
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    pattern: Regex,
    include_globs: Option<GlobSet>,
    exclude_files: Option<GlobSet>,
    config_dir: PathBuf,
    message: String,
    remediation: String,
    severity: Severity,
}

impl CompiledRule {
    fn applies_to(&self, path: &Path) -> bool {
        if let Some(exclude_files) = &self.exclude_files
            && is_excluded(path, exclude_files, &self.config_dir)
        {
            return false;
        }
        if let Some(include_globs) = &self.include_globs {
            return include_globs.is_match(path);
        }
        true
    }
}

fn parse_config(config: &toml::Value, config_dir: &Path) -> Result<CompiledForbiddenImportsDepsConfig> {
    let parsed: ForbiddenImportsDepsConfig = config
        .clone()
        .try_into()
        .context("invalid forbidden-imports-deps config")?;
    if parsed.rules.is_empty() {
        bail!("forbidden-imports-deps config must contain at least one `rules` entry");
    }

    let default_severity = Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_remediation = parsed
        .remediation
        .unwrap_or_else(|| "Replace the forbidden import/dependency usage with approved project patterns.".to_owned());

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for rule in parsed.rules {
        let pattern = Regex::new(&rule.pattern).with_context(|| format!("invalid rule regex: {}", rule.pattern))?;
        rules.push(CompiledRule {
            pattern,
            include_globs: compile_globs("include_globs", &rule.include_globs)?,
            exclude_files: compile_globs("exclude_files", &rule.exclude_files)?,
            config_dir: config_dir.to_path_buf(),
            message: rule.message,
            remediation: rule.remediation.unwrap_or_else(|| default_remediation.clone()),
            severity: Severity::parse_with_default(rule.severity.as_deref(), default_severity),
        });
    }

    Ok(CompiledForbiddenImportsDepsConfig { rules })
}

/// Returns true if `path` is within `config_dir` and matches `globs` (relative to config_dir).
/// Files outside the config_dir subtree are never excluded.
fn is_excluded(path: &Path, globs: &GlobSet, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    globs.is_match(relative)
}

fn compile_globs(field_name: &str, patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    let globset = builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` globs"))?;
    Ok(Some(globset))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::ForbiddenImportsDepsCheck;

    #[tokio::test]
    async fn flags_forbidden_pattern_in_included_file() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("frontend/src/components")).expect("create dirs");
        fs::write(
            temp.path().join("frontend/src/components/Foo.tsx"),
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
        )
        .expect("write source");

        let check = ForbiddenImportsDepsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/components/Foo.tsx").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        pattern = "\\bfetch\\(url\\(",
                        message = "Use frontend api/* modules for backend calls.",
                        include_globs = ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                        exclude_globs = ["frontend/src/api/**"]
                    }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn ignores_excluded_paths() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("frontend/src/api")).expect("create dirs");
        fs::write(
            temp.path().join("frontend/src/api/http.ts"),
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
        )
        .expect("write source");

        let check = ForbiddenImportsDepsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/api/http.ts").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        pattern = "\\bfetch\\(url\\(",
                        message = "Use frontend api/* modules for backend calls.",
                        include_globs = ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                        exclude_files = ["frontend/src/api/**"]
                    }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn exclude_globs_alias_still_works() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("frontend/src/api")).expect("create dirs");
        fs::write(
            temp.path().join("frontend/src/api/http.ts"),
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
        )
        .expect("write source");

        let check = ForbiddenImportsDepsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/api/http.ts").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{
                        pattern = "\\bfetch\\(url\\(",
                        message = "Use frontend api/* modules for backend calls.",
                        include_globs = ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                        exclude_globs = ["frontend/src/api/**"]
                    }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
