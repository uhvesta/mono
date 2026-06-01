use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct TodoExpiryCheck;

#[async_trait]
impl Check for TodoExpiryCheck {
    fn id(&self) -> &str {
        "todo-expiry"
    }

    fn description(&self) -> &str {
        "requires TODO/FIXME annotations to include owner and date tags"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledTodoExpiryConfig {
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
                if !self.todo_detector.is_match(line) {
                    continue;
                }
                if self.required_format.is_match(line) {
                    continue;
                }

                findings.push(Finding {
                    severity: self.severity,
                    message: "TODO/FIXME must include `(@owner,YYYY-MM-DD)` metadata".to_owned(),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: Some((line_index + 1) as u32),
                        column: Some(1),
                    }),
                    remediations: vec![self.remediation.clone()],
                    suggested_fix: None,
                });
            }
        }

        Ok(CheckResult {
            check_id: "todo-expiry".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct TodoExpiryConfig {
    #[serde(default)]
    required_pattern: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledTodoExpiryConfig {
    todo_detector: Regex,
    required_format: Regex,
    severity: Severity,
    remediation: String,
}

fn parse_config(config: &toml::Value) -> Result<CompiledTodoExpiryConfig> {
    let parsed: TodoExpiryConfig = config
        .clone()
        .try_into()
        .context("invalid todo-expiry config")?;

    let required_pattern = parsed.required_pattern.unwrap_or_else(|| {
        r"(?i)\b(?:TODO|FIXME)\s*\(@[A-Za-z0-9._-]+,\s*\d{4}-\d{2}-\d{2}\)\s*:".to_owned()
    });
    let required_format = Regex::new(&required_pattern)
        .with_context(|| format!("invalid required_pattern regex: {required_pattern}"))?;

    Ok(CompiledTodoExpiryConfig {
        todo_detector: Regex::new(
            r"(?i)^\s*(?://|#|/\*|\*|<!--)\s*(?:TODO|FIXME)\s*(?:\([^)]*\)\s*)?:",
        )
        .expect("valid detector regex"),
        required_format,
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Warning),
        remediation: parsed.remediation.unwrap_or_else(|| {
            "Use format `TODO(@owner,YYYY-MM-DD): ...` or `FIXME(@owner,YYYY-MM-DD): ...`."
                .to_owned()
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::TodoExpiryCheck;

    #[tokio::test]
    async fn flags_todo_without_owner_and_date() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("notes.txt"),
            "// TODO: clean this up later\n",
        )
        .expect("write file");

        let check = TodoExpiryCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("notes.txt").to_path_buf(),
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
    async fn accepts_todo_with_owner_and_date() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("notes.txt"),
            "// TODO(@brian,2026-02-14): clean this up later\n",
        )
        .expect("write file");

        let check = TodoExpiryCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("notes.txt").to_path_buf(),
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
}
