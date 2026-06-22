use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct WorkflowActionVersionCheck;

#[async_trait]
impl Check for WorkflowActionVersionCheck {
    fn id(&self) -> &str {
        "workflow-action-version"
    }

    fn description(&self) -> &str {
        "requires configured GitHub Actions `uses:` refs to match expected versions"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledWorkflowActionVersionConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        changeset
            .changed_files
            .iter()
            .filter(|f| !matches!(f.kind, ChangeKind::Deleted) && is_github_workflow_file(&f.path))
            .count()
    }

    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        self.run_with_progress(changeset, tree, Arc::new(|_| {})).await
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let mut findings = Vec::new();
        let mut processed = 0usize;

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_github_workflow_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                processed += 1;
                on_file_processed(processed);
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                processed += 1;
                on_file_processed(processed);
                continue;
            };

            let workflow = match parse_workflow(&contents) {
                Ok(workflow) => workflow,
                Err(error) => {
                    findings.push(Finding {
                        severity: Severity::Error,
                        message: format!("failed to parse workflow YAML while enforcing action versions: {error}"),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec![
                            "Fix YAML syntax so checks can validate `uses:` action versions.".to_owned(),
                        ],
                        suggested_fix: None,
                    });
                    processed += 1;
                    on_file_processed(processed);
                    continue;
                }
            };

            for violation in find_version_violations(&workflow, &self.rules) {
                findings.push(Finding {
                    severity: self.severity,
                    message: format!(
                        "GitHub Action `{}` in job `{}` step {} must use `@{}` (found `@{}`).",
                        violation.action,
                        violation.job_name,
                        violation.step_index,
                        violation.expected_version,
                        violation.actual_version
                    ),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: None,
                        column: None,
                    }),
                    remediations: vec![self.remediation.clone()],
                    suggested_fix: None,
                });
            }
            processed += 1;
            on_file_processed(processed);
        }

        Ok(CheckResult {
            check_id: "workflow-action-version".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowActionVersionConfig {
    #[serde(default)]
    rules: Vec<WorkflowActionVersionRule>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkflowActionVersionRule {
    action: String,
    version: String,
}

#[derive(Debug)]
struct CompiledWorkflowActionVersionConfig {
    rules: Vec<WorkflowActionVersionRule>,
    severity: Severity,
    remediation: String,
}

#[derive(Debug)]
struct WorkflowActionVersionViolation {
    action: String,
    expected_version: String,
    actual_version: String,
    job_name: String,
    step_index: usize,
}

fn parse_config(config: &toml::Value) -> Result<CompiledWorkflowActionVersionConfig> {
    let parsed: WorkflowActionVersionConfig = config
        .clone()
        .try_into()
        .context("invalid workflow-action-version check config")?;
    if parsed.rules.is_empty() {
        bail!("workflow-action-version check config must contain at least one `rules` entry");
    }

    Ok(CompiledWorkflowActionVersionConfig {
        rules: parsed.rules,
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
        remediation: parsed
            .remediation
            .unwrap_or_else(|| "Pin GitHub Actions `uses:` references to the configured version.".to_owned()),
    })
}

fn is_github_workflow_file(path: &Path) -> bool {
    if !path.starts_with(Path::new(".github/workflows")) {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yml") | Some("yaml")
    )
}

fn parse_workflow(contents: &str) -> Result<Value> {
    serde_yaml::from_str(contents).context("invalid YAML document")
}

fn find_version_violations(
    workflow: &Value,
    rules: &[WorkflowActionVersionRule],
) -> Vec<WorkflowActionVersionViolation> {
    let mut violations = Vec::new();
    let Some(root) = workflow.as_mapping() else {
        return violations;
    };
    let Some(jobs) = mapping_get(root, "jobs").and_then(Value::as_mapping) else {
        return violations;
    };

    for (job_key, job_value) in jobs {
        let Some(job) = job_value.as_mapping() else {
            continue;
        };
        let Some(steps) = mapping_get(job, "steps").and_then(Value::as_sequence) else {
            continue;
        };
        let job_name = job_key.as_str().unwrap_or("<unknown-job>").to_owned();

        for (index, step) in steps.iter().enumerate() {
            let Some(step_map) = step.as_mapping() else {
                continue;
            };
            let Some(uses) = mapping_get(step_map, "uses").and_then(Value::as_str) else {
                continue;
            };
            let Some((action, version)) = parse_uses_ref(uses) else {
                continue;
            };

            for rule in rules {
                if action != rule.action {
                    continue;
                }
                if version == rule.version {
                    continue;
                }

                violations.push(WorkflowActionVersionViolation {
                    action: action.to_owned(),
                    expected_version: rule.version.clone(),
                    actual_version: version.to_owned(),
                    job_name: job_name.clone(),
                    step_index: index + 1,
                });
            }
        }
    }

    violations
}

fn parse_uses_ref(uses: &str) -> Option<(&str, &str)> {
    let (action, version) = uses.rsplit_once('@')?;
    Some((action.trim(), version.trim()))
}

fn mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(Value::String(key.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::WorkflowActionVersionCheck;

    #[tokio::test]
    async fn flags_checkout_version_mismatch() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflow dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - uses: actions/checkout@v3
"#,
        )
        .expect("write workflow");

        let check = WorkflowActionVersionCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ action = "actions/checkout", version = "v4" }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("actions/checkout"));
        assert!(result.findings[0].message.contains("@v4"));
        assert!(result.findings[0].message.contains("@v3"));
    }

    #[tokio::test]
    async fn accepts_matching_versions() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflow dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - uses: actions/checkout@v4
"#,
        )
        .expect("write workflow");

        let check = WorkflowActionVersionCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ action = "actions/checkout", version = "v4" }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn requires_rules_config() {
        let temp = tempdir().expect("create temp dir");
        let check = WorkflowActionVersionCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
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
    async fn ignores_non_workflow_paths() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("README.md"), "hello").expect("write readme");

        let check = WorkflowActionVersionCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("README.md").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ action = "actions/checkout", version = "v4" }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
