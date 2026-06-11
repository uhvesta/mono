use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_yaml::{Mapping, Value};

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

const STRICT_MODE_PREFIX: &str = "set -euo pipefail";

#[derive(Debug, Default)]
pub struct WorkflowShellStrictCheck;

#[async_trait]
impl Check for WorkflowShellStrictCheck {
    fn id(&self) -> &str {
        "workflow-shell-strict"
    }

    fn description(&self) -> &str {
        "requires GitHub Actions run scripts to start with strict shell mode"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for WorkflowShellStrictCheck {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_github_workflow_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                continue;
            };

            let workflow = match parse_workflow(&contents) {
                Ok(workflow) => workflow,
                Err(error) => {
                    findings.push(Finding {
                        severity: Severity::Error,
                        message: format!("failed to parse workflow YAML while enforcing strict shell mode: {error}"),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec!["Fix YAML syntax so checks can validate `run:` script blocks.".to_owned()],
                        suggested_fix: None,
                    });
                    continue;
                }
            };

            for violation in find_non_strict_run_scripts(&workflow) {
                findings.push(Finding {
                    severity: Severity::Error,
                    message: format!(
                        "GitHub Actions run script in job `{}` step {} must start with `set -euo pipefail`.",
                        violation.job_name, violation.step_index
                    ),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: None,
                        column: None,
                    }),
                    remediations: vec![
                        "Add `set -euo pipefail` as the first non-comment line in each `run:` script block.".to_owned(),
                    ],
                    suggested_fix: None,
                });
            }
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

#[derive(Debug)]
struct RunScriptViolation {
    job_name: String,
    step_index: usize,
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

fn find_non_strict_run_scripts(workflow: &Value) -> Vec<RunScriptViolation> {
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
            let Some(run_script) = mapping_get(step_map, "run").and_then(Value::as_str) else {
                continue;
            };
            if !is_multiline_script(run_script) {
                continue;
            }
            if !starts_with_strict_mode(run_script) {
                violations.push(RunScriptViolation {
                    job_name: job_name.clone(),
                    step_index: index + 1,
                });
            }
        }
    }

    violations
}

fn mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(Value::String(key.to_owned()))
}

fn is_multiline_script(script: &str) -> bool {
    script.contains('\n')
}

fn starts_with_strict_mode(script: &str) -> bool {
    let first_command = script
        .lines()
        .map(str::trim_start)
        .find(|line| !line.is_empty() && !line.starts_with('#'));

    first_command
        .map(|line| line.starts_with(STRICT_MODE_PREFIX))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::WorkflowShellStrictCheck;

    #[tokio::test]
    async fn flags_missing_strict_mode_in_workflow_run_block() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
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
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(
            result.findings[0]
                .message
                .contains("job `test` step 1 must start with `set -euo pipefail`")
        );
    }

    #[tokio::test]
    async fn accepts_strict_mode_after_comments_and_blank_lines() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yml"),
            r#"jobs:
  test:
    steps:
      - run: |

          # strict shell mode
          set -euo pipefail
          echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yml").to_path_buf(),
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
    async fn ignores_non_workflow_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("create docs dir");
        fs::write(
            temp.path().join("docs/example.yaml"),
            r#"run: |
  echo "hello"
"#,
        )
        .expect("write yaml");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/example.yaml").to_path_buf(),
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
    async fn ignores_single_line_run_entries() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
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
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn reports_yaml_parse_failures() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          echo "hello"
      - bad: [unclosed
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
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
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("failed to parse workflow YAML"));
    }
}
