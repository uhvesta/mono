use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct WorkflowRunPatternsCheck;

#[async_trait]
impl Check for WorkflowRunPatternsCheck {
    fn id(&self) -> &str {
        "workflow-run-patterns"
    }

    fn description(&self) -> &str {
        "flags GitHub workflow run scripts that match configured regex patterns"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledWorkflowRunPatternsConfig {
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
                        message: format!(
                            "failed to parse workflow YAML while checking run patterns: {error}"
                        ),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec![
                            "Fix YAML syntax so checks can validate workflow `run:` blocks."
                                .to_owned(),
                        ],
                        suggested_fix: None,
                    });
                    continue;
                }
            };

            for script in list_run_scripts(&workflow) {
                for rule in &self.rules {
                    if !script_violates_rule(script.script, rule) {
                        continue;
                    }
                    findings.push(Finding {
                        severity: rule.severity,
                        message: format!(
                            "GitHub Actions run script in job `{}` step {}: {}",
                            script.job_name, script.step_index, rule.message
                        ),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec![rule.remediation.clone()],
                        suggested_fix: None,
                    });
                }
            }
        }

        Ok(CheckResult {
            check_id: "workflow-run-patterns".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowRunPatternsConfig {
    #[serde(default)]
    rules: Vec<WorkflowRunPatternRuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunPatternRuleConfig {
    pattern: String,
    message: String,
    #[serde(default)]
    must_include: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug)]
struct WorkflowRunPatternRule {
    pattern: Regex,
    message: String,
    must_include: Vec<String>,
    severity: Severity,
    remediation: String,
}

#[derive(Debug)]
struct CompiledWorkflowRunPatternsConfig {
    rules: Vec<WorkflowRunPatternRule>,
}

#[derive(Debug)]
struct WorkflowRunScript<'a> {
    job_name: String,
    step_index: usize,
    script: &'a str,
}

fn parse_config(config: &toml::Value) -> Result<CompiledWorkflowRunPatternsConfig> {
    let parsed: WorkflowRunPatternsConfig = config
        .clone()
        .try_into()
        .context("invalid workflow-run-patterns check config")?;
    if parsed.rules.is_empty() {
        bail!("workflow-run-patterns check config must contain at least one `rules` entry");
    }

    let default_severity =
        Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_remediation = parsed.remediation.unwrap_or_else(|| {
        "Update the workflow run script to satisfy repository CI conventions.".to_owned()
    });

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for rule in parsed.rules {
        let pattern = Regex::new(&rule.pattern)
            .with_context(|| format!("invalid rule regex: {}", rule.pattern))?;
        rules.push(WorkflowRunPatternRule {
            pattern,
            message: rule.message,
            must_include: rule.must_include,
            severity: Severity::parse_with_default(rule.severity.as_deref(), default_severity),
            remediation: rule
                .remediation
                .unwrap_or_else(|| default_remediation.clone()),
        });
    }

    Ok(CompiledWorkflowRunPatternsConfig { rules })
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

fn list_run_scripts(workflow: &Value) -> Vec<WorkflowRunScript<'_>> {
    let mut scripts = Vec::new();
    let Some(root) = workflow.as_mapping() else {
        return scripts;
    };
    let Some(jobs) = mapping_get(root, "jobs").and_then(Value::as_mapping) else {
        return scripts;
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
            let Some(script) = mapping_get(step_map, "run").and_then(Value::as_str) else {
                continue;
            };
            scripts.push(WorkflowRunScript {
                job_name: job_name.clone(),
                step_index: index + 1,
                script,
            });
        }
    }

    scripts
}

fn script_violates_rule(script: &str, rule: &WorkflowRunPatternRule) -> bool {
    if rule.must_include.is_empty() {
        return rule.pattern.is_match(script);
    }

    script.lines().map(str::trim).any(|line| {
        if !rule.pattern.is_match(line) {
            return false;
        }
        rule.must_include.iter().any(|token| !line.contains(token))
    })
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

    use super::WorkflowRunPatternsCheck;

    #[tokio::test]
    async fn flags_matching_run_script_pattern() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflow dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          curl https://example.com/script.sh -o script.sh
"#,
        )
        .expect("write workflow");

        let check = WorkflowRunPatternsCheck;
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
                    rules = [{ pattern = "\\bcurl\\b", must_include = ["-f"], message = "Use curl -fsSL." }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("Use curl -fsSL."));
    }

    #[tokio::test]
    async fn ignores_non_matching_scripts() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflow dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          curl -fsSL https://example.com/script.sh -o script.sh
"#,
        )
        .expect("write workflow");

        let check = WorkflowRunPatternsCheck;
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
                    rules = [{ pattern = "\\bcurl\\b", must_include = ["-f"], message = "Use curl -fsSL." }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn requires_rules_config() {
        let temp = tempdir().expect("create temp dir");
        let check = WorkflowRunPatternsCheck;
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
    async fn rejects_invalid_regex() {
        let temp = tempdir().expect("create temp dir");
        let check = WorkflowRunPatternsCheck;
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
                    rules = [{ pattern = "(", message = "bad regex" }]
                }),
            )
            .await;

        assert!(result.is_err());
    }
}
