use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub(crate) struct BazelversionPoliciesCheck;

#[async_trait]
impl Check for BazelversionPoliciesCheck {
    fn id(&self) -> &str {
        "bazelversion-policies"
    }

    fn description(&self) -> &str {
        "flags configured .bazelversion policy violations in changed files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledBazelversionPoliciesConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if changed_file.path != Path::new(".bazelversion") {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };

            let version = contents.trim();
            for rule in &self.rules {
                if let Some(finding) = rule.evaluate(&changed_file.path, version) {
                    findings.push(finding);
                }
            }
        }

        Ok(CheckResult {
            check_id: "bazelversion-policies".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BazelversionPoliciesConfig {
    #[serde(default)]
    rules: Vec<BazelversionPolicyRuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BazelversionPolicyRuleConfig {
    AllowedVersionPatterns {
        patterns: Vec<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
}

#[derive(Debug)]
struct CompiledBazelversionPoliciesConfig {
    rules: Vec<CompiledRule>,
}

#[derive(Debug)]
enum CompiledRule {
    AllowedVersionPatterns(CompiledAllowedVersionPatternsRule),
}

#[derive(Debug)]
struct CompiledAllowedVersionPatternsRule {
    pattern_strings: Vec<String>,
    patterns: Vec<GlobMatcher>,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

impl CompiledRule {
    fn evaluate(&self, path: &Path, version: &str) -> Option<Finding> {
        match self {
            Self::AllowedVersionPatterns(rule) => rule.evaluate(path, version),
        }
    }
}

impl CompiledAllowedVersionPatternsRule {
    fn evaluate(&self, path: &Path, version: &str) -> Option<Finding> {
        if self
            .patterns
            .iter()
            .any(|pattern| pattern.is_match(Path::new(version)))
        {
            return None;
        }

        Some(Finding {
            severity: self.severity,
            message: self.message.clone().unwrap_or_else(|| {
                format!(
                    "`.bazelversion` value `{version}` must match one of: {}.",
                    self.pattern_strings
                        .iter()
                        .map(|pattern| format!("`{pattern}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
            location: Some(Location {
                path: path.to_path_buf(),
                line: Some(1),
                column: Some(1),
            }),
            remediations: vec![self.remediation.clone().unwrap_or_else(|| {
                format!(
                    "Update `.bazelversion` so it matches one of the approved patterns: {}.",
                    self.pattern_strings.join(", ")
                )
            })],
            suggested_fix: None,
        })
    }
}

fn parse_config(config: &toml::Value) -> Result<CompiledBazelversionPoliciesConfig> {
    let parsed: BazelversionPoliciesConfig = config
        .clone()
        .try_into()
        .context("invalid bazelversion-policies check config")?;
    if parsed.rules.is_empty() {
        bail!("bazelversion-policies check config must contain at least one `rules` entry");
    }

    let default_severity =
        Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_remediation = normalize_optional_string(parsed.remediation, "remediation")?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for (index, rule) in parsed.rules.into_iter().enumerate() {
        let field_prefix = format!("rules[{index}]");
        rules.push(match rule {
            BazelversionPolicyRuleConfig::AllowedVersionPatterns {
                patterns,
                message,
                remediation,
                severity,
            } => {
                let pattern_strings = normalize_non_empty_unique_strings(
                    patterns,
                    &format!("{field_prefix}.patterns"),
                )?;
                let patterns =
                    compile_patterns(&format!("{field_prefix}.patterns"), &pattern_strings)?;
                CompiledRule::AllowedVersionPatterns(CompiledAllowedVersionPatternsRule {
                    pattern_strings,
                    patterns,
                    message: normalize_optional_string(
                        message,
                        &format!("{field_prefix}.message"),
                    )?,
                    remediation: normalize_optional_string(
                        remediation,
                        &format!("{field_prefix}.remediation"),
                    )?
                    .or_else(|| default_remediation.clone()),
                    severity: Severity::parse_with_default(severity.as_deref(), default_severity),
                })
            }
        });
    }

    Ok(CompiledBazelversionPoliciesConfig { rules })
}

fn normalize_optional_string(value: Option<String>, field_name: &str) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("bazelversion-policies check config `{field_name}` must not be empty when present");
    }
    Ok(Some(trimmed.to_owned()))
}

fn normalize_non_empty_unique_strings(
    values: Vec<String>,
    field_name: &str,
) -> Result<Vec<String>> {
    if values.is_empty() {
        bail!("bazelversion-policies check config `{field_name}` must contain at least one value");
    }

    let mut seen = std::collections::HashSet::new();
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!(
                "bazelversion-policies check config `{field_name}` must not contain empty values"
            );
        }
        if seen.insert(trimmed.to_owned()) {
            output.push(trimmed.to_owned());
        }
    }
    Ok(output)
}

fn compile_patterns(field_name: &str, patterns: &[String]) -> Result<Vec<GlobMatcher>> {
    patterns
        .iter()
        .map(|pattern| {
            Glob::new(pattern)
                .with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))
                .map(|glob| glob.compile_matcher())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::BazelversionPoliciesCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    #[tokio::test]
    async fn passes_when_version_matches_exact_pattern() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join(".bazelversion"), "channel:live\n").expect("write version");

        let check = BazelversionPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelversion").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "allowed_version_patterns", patterns = ["channel:live", "channel:alpha"] }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn passes_when_version_matches_wildcard_pattern() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join(".bazelversion"), "8.4.0\n").expect("write version");

        let check = BazelversionPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelversion").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "allowed_version_patterns", patterns = ["channel:*", "8.*"] }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn reports_version_that_does_not_match_allowed_patterns() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join(".bazelversion"), "channel:beta\n").expect("write version");

        let check = BazelversionPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelversion").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "allowed_version_patterns", patterns = ["channel:live", "channel:alpha", "8.*"] }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(
            result.findings[0]
                .message
                .contains("must match one of: `channel:live`, `channel:alpha`, `8.*`")
        );
    }

    #[tokio::test]
    async fn rejects_invalid_glob_pattern() {
        let check = BazelversionPoliciesCheck;
        let err = match check.configure(&toml::Value::Table(toml::toml! {
            rules = [{ kind = "allowed_version_patterns", patterns = ["["] }]
        })) {
            Ok(_) => panic!("config should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("invalid `rules[0].patterns` glob pattern: [")
        );
    }
}
