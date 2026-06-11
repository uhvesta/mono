use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct ApiBreakingSurfaceCheck;

#[async_trait]
impl Check for ApiBreakingSurfaceCheck {
    fn id(&self) -> &str {
        "api-breaking-surface"
    }

    fn description(&self) -> &str {
        "requires API-facing backend changes to include configured documentation/version marker updates"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledApiBreakingSurfaceConfig {
    async fn run(&self, changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut trigger_files = Vec::new();
        let mut required_updated = false;

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            if self.required_globs.is_match(&changed_file.path) {
                required_updated = true;
            }
            if self.trigger_globs.is_match(&changed_file.path) {
                trigger_files.push(changed_file.path.clone());
            }
        }

        if trigger_files.is_empty() || required_updated {
            return Ok(CheckResult {
                check_id: "api-breaking-surface".to_owned(),
                findings: Vec::new(),
            });
        }

        let findings = trigger_files
            .into_iter()
            .map(|path| Finding {
                severity: Severity::Error,
                message: self.message.clone(),
                location: Some(Location {
                    path,
                    line: None,
                    column: None,
                }),
                remediations: vec![self.remediation.clone()],
                suggested_fix: None,
            })
            .collect();

        Ok(CheckResult {
            check_id: "api-breaking-surface".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ApiBreakingSurfaceConfig {
    #[serde(default)]
    trigger_globs: Vec<String>,
    #[serde(default)]
    required_globs: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledApiBreakingSurfaceConfig {
    trigger_globs: GlobSet,
    required_globs: GlobSet,
    message: String,
    remediation: String,
}

fn parse_config(config: &toml::Value) -> Result<CompiledApiBreakingSurfaceConfig> {
    let parsed: ApiBreakingSurfaceConfig = config
        .clone()
        .try_into()
        .context("invalid api-breaking-surface config")?;

    if parsed.trigger_globs.is_empty() {
        bail!("api-breaking-surface config must define `trigger_globs`");
    }
    if parsed.required_globs.is_empty() {
        bail!("api-breaking-surface config must define `required_globs`");
    }

    Ok(CompiledApiBreakingSurfaceConfig {
        trigger_globs: compile_globs("trigger_globs", &parsed.trigger_globs)?,
        required_globs: compile_globs("required_globs", &parsed.required_globs)?,
        message: parsed.message.unwrap_or_else(|| {
            "backend API surface changed without required changelog/version marker update".to_owned()
        }),
        remediation: parsed
            .remediation
            .unwrap_or_else(|| "Update the configured companion docs/version marker files in this change.".to_owned()),
    })
}

fn compile_globs(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` globs"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;
    use tempfile::tempdir;

    use super::ApiBreakingSurfaceCheck;

    #[tokio::test]
    async fn flags_trigger_change_without_required_update() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn passes_when_required_file_is_updated() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![
                    ChangedFile {
                        path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
                        kind: ChangeKind::Modified,
                        old_path: None,
                    },
                    ChangedFile {
                        path: Path::new("docs/backend.md").to_path_buf(),
                        kind: ChangeKind::Modified,
                        old_path: None,
                    },
                ]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_backend_changes_outside_trigger_globs() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/blob/src/v2/fencer.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = [
                        "backend/blob/src/app.rs",
                        "backend/blob/src/v2/mod.rs",
                        "backend/blob/src/v2/model.rs",
                    ]
                    required_globs = ["docs/backend.md"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
