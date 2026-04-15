use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

use super::rc_parser::{
    BazelrcEntry, BazelrcEntryKind, ParsedBazelrcClosure, is_bazelrc_root_candidate,
    parse_bazelrc_closure,
};

#[derive(Debug, Default)]
pub(crate) struct BazelrcPoliciesCheck;

#[async_trait]
impl Check for BazelrcPoliciesCheck {
    fn id(&self) -> &str {
        "bazelrc-policies"
    }

    fn description(&self) -> &str {
        "flags configured Bazel rc policy violations in changed rc files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledBazelrcPoliciesConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let changed_paths: HashSet<PathBuf> = changeset
            .changed_files
            .iter()
            .filter(|changed_file| !matches!(changed_file.kind, ChangeKind::Deleted))
            .map(|changed_file| changed_file.path.clone())
            .collect();

        let mut findings = Vec::new();
        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_bazelrc_root_candidate(&changed_file.path) {
                continue;
            }

            let Ok(parsed) = parse_bazelrc_closure(&changed_file.path, tree) else {
                continue;
            };

            for rule in &self.rules {
                findings.extend(rule.evaluate(&changed_file.path, &parsed, &changed_paths));
            }
        }

        Ok(CheckResult {
            check_id: "bazelrc-policies".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BazelrcPoliciesConfig {
    #[serde(default)]
    rules: Vec<BazelrcPolicyRuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BazelrcPolicyRuleConfig {
    RequiredFlag {
        commands: Vec<String>,
        flag: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
    ForbiddenFlag {
        commands: Vec<String>,
        flag: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        remediation: Option<String>,
        #[serde(default)]
        severity: Option<String>,
    },
}

#[derive(Debug)]
struct CompiledBazelrcPoliciesConfig {
    rules: Vec<CompiledRule>,
}

#[derive(Debug)]
enum CompiledRule {
    RequiredFlag(CompiledRequiredFlagRule),
    ForbiddenFlag(CompiledForbiddenFlagRule),
}

#[derive(Debug)]
struct CompiledRequiredFlagRule {
    commands: Vec<String>,
    flag: String,
    value: Option<String>,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

#[derive(Debug)]
struct CompiledForbiddenFlagRule {
    commands: Vec<String>,
    flag: String,
    message: Option<String>,
    remediation: Option<String>,
    severity: Severity,
}

impl CompiledRule {
    fn evaluate(
        &self,
        root_path: &Path,
        parsed: &ParsedBazelrcClosure,
        changed_paths: &HashSet<PathBuf>,
    ) -> Vec<Finding> {
        match self {
            Self::RequiredFlag(rule) => rule.evaluate(root_path, parsed),
            Self::ForbiddenFlag(rule) => rule.evaluate(parsed, changed_paths),
        }
    }
}

impl CompiledRequiredFlagRule {
    fn evaluate(&self, root_path: &Path, parsed: &ParsedBazelrcClosure) -> Vec<Finding> {
        if parsed.entries.iter().any(|entry| self.matches(entry)) {
            return Vec::new();
        }

        vec![Finding {
            severity: self.severity,
            message: self.message.clone().unwrap_or_else(|| match &self.value {
                Some(value) => format!(
                    "Bazel rc files applicable to `{}` must declare `--{}={value}`.",
                    self.commands.join(", "),
                    self.flag,
                ),
                None => format!(
                    "Bazel rc files applicable to `{}` must declare `--{}`.",
                    self.commands.join(", "),
                    self.flag,
                ),
            }),
            location: Some(Location {
                path: root_path.to_path_buf(),
                line: None,
                column: None,
            }),
            remediation: Some(
                self.remediation
                    .clone()
                    .unwrap_or_else(|| match &self.value {
                        Some(value) => format!(
                            "Update the Bazel rc configuration so `{}` declares `--{}={value}`.",
                            self.commands.join(", "),
                            self.flag,
                        ),
                        None => format!(
                            "Update the Bazel rc configuration so `{}` declares `--{}`.",
                            self.commands.join(", "),
                            self.flag,
                        ),
                    }),
            ),
            suggested_fix: None,
        }]
    }

    fn matches(&self, entry: &BazelrcEntry) -> bool {
        if entry.kind != BazelrcEntryKind::Flag {
            return false;
        }
        if entry.config_name.is_some() {
            return false;
        }
        if entry.flag.as_deref() != Some(self.flag.as_str()) {
            return false;
        }
        if !entry_applies_to_commands(entry.command.as_deref(), &self.commands) {
            return false;
        }
        match &self.value {
            Some(value) => entry.value.as_deref() == Some(value.as_str()),
            None => true,
        }
    }
}

impl CompiledForbiddenFlagRule {
    fn evaluate(
        &self,
        parsed: &ParsedBazelrcClosure,
        changed_paths: &HashSet<PathBuf>,
    ) -> Vec<Finding> {
        parsed
            .entries
            .iter()
            .filter(|entry| self.matches(entry))
            .filter(|entry| changed_paths.contains(&entry.source_path))
            .map(|entry| Finding {
                severity: self.severity,
                message: self.message.clone().unwrap_or_else(|| {
                    format!(
                        "Do not declare `--{}` in Bazel rc files applicable to `{}`.",
                        self.flag,
                        self.commands.join(", "),
                    )
                }),
                location: Some(Location {
                    path: entry.source_path.clone(),
                    line: Some(entry.line),
                    column: Some(entry.column),
                }),
                remediation: Some(self.remediation.clone().unwrap_or_else(|| {
                    format!(
                        "Remove the `--{}` flag or switch to the approved alternative.",
                        self.flag
                    )
                })),
                suggested_fix: None,
            })
            .collect()
    }

    fn matches(&self, entry: &BazelrcEntry) -> bool {
        entry.kind == BazelrcEntryKind::Flag
            && entry.config_name.is_none()
            && entry.flag.as_deref() == Some(self.flag.as_str())
            && entry_applies_to_commands(entry.command.as_deref(), &self.commands)
    }
}

fn parse_config(config: &toml::Value) -> Result<CompiledBazelrcPoliciesConfig> {
    let parsed: BazelrcPoliciesConfig = config
        .clone()
        .try_into()
        .context("invalid bazelrc-policies check config")?;
    if parsed.rules.is_empty() {
        bail!("bazelrc-policies check config must contain at least one `rules` entry");
    }

    let default_severity =
        Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_remediation = normalize_optional_string(parsed.remediation, "remediation")?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for (index, rule) in parsed.rules.into_iter().enumerate() {
        let field_prefix = format!("rules[{index}]");
        rules.push(match rule {
            BazelrcPolicyRuleConfig::RequiredFlag {
                commands,
                flag,
                value,
                message,
                remediation,
                severity,
            } => CompiledRule::RequiredFlag(CompiledRequiredFlagRule {
                commands: normalize_commands(commands, &format!("{field_prefix}.commands"))?,
                flag: normalize_flag_name(flag, &format!("{field_prefix}.flag"))?,
                value: normalize_optional_string(value, &format!("{field_prefix}.value"))?,
                message: normalize_optional_string(message, &format!("{field_prefix}.message"))?,
                remediation: normalize_optional_string(
                    remediation,
                    &format!("{field_prefix}.remediation"),
                )?
                .or_else(|| default_remediation.clone()),
                severity: Severity::parse_with_default(severity.as_deref(), default_severity),
            }),
            BazelrcPolicyRuleConfig::ForbiddenFlag {
                commands,
                flag,
                message,
                remediation,
                severity,
            } => CompiledRule::ForbiddenFlag(CompiledForbiddenFlagRule {
                commands: normalize_commands(commands, &format!("{field_prefix}.commands"))?,
                flag: normalize_flag_name(flag, &format!("{field_prefix}.flag"))?,
                message: normalize_optional_string(message, &format!("{field_prefix}.message"))?,
                remediation: normalize_optional_string(
                    remediation,
                    &format!("{field_prefix}.remediation"),
                )?
                .or_else(|| default_remediation.clone()),
                severity: Severity::parse_with_default(severity.as_deref(), default_severity),
            }),
        });
    }

    Ok(CompiledBazelrcPoliciesConfig { rules })
}

fn normalize_optional_string(value: Option<String>, field_name: &str) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("bazelrc-policies check config `{field_name}` must not be empty when present");
    }
    Ok(Some(trimmed.to_owned()))
}

fn normalize_commands(commands: Vec<String>, field_name: &str) -> Result<Vec<String>> {
    if commands.is_empty() {
        bail!("bazelrc-policies check config `{field_name}` must contain at least one command");
    }

    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(commands.len());
    for command in commands {
        let trimmed = command.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            bail!("bazelrc-policies check config `{field_name}` must not contain empty commands");
        }
        if seen.insert(trimmed.clone()) {
            output.push(trimmed);
        }
    }
    Ok(output)
}

fn normalize_flag_name(flag: String, field_name: &str) -> Result<String> {
    let trimmed = flag.trim().trim_start_matches('-').trim();
    if trimmed.is_empty() {
        bail!("bazelrc-policies check config `{field_name}` must not be empty");
    }
    Ok(trimmed.to_owned())
}

fn entry_applies_to_commands(entry_command: Option<&str>, requested_commands: &[String]) -> bool {
    let Some(entry_command) = entry_command else {
        return false;
    };

    requested_commands
        .iter()
        .any(|requested| entry_command_applies_to_requested(entry_command, requested))
}

fn entry_command_applies_to_requested(entry_command: &str, requested_command: &str) -> bool {
    match requested_command {
        "always" => entry_command == "always",
        "common" => matches!(entry_command, "common" | "always"),
        "startup" => entry_command == "startup",
        "coverage" => matches!(
            entry_command,
            "coverage" | "test" | "build" | "common" | "always"
        ),
        "test" => matches!(entry_command, "test" | "build" | "common" | "always"),
        "run" => matches!(entry_command, "run" | "build" | "common" | "always"),
        "clean" => matches!(entry_command, "clean" | "build" | "common" | "always"),
        "mobile-install" => {
            matches!(
                entry_command,
                "mobile-install" | "build" | "common" | "always"
            )
        }
        "info" => matches!(entry_command, "info" | "build" | "common" | "always"),
        "print_action" => matches!(
            entry_command,
            "print_action" | "build" | "common" | "always"
        ),
        "config" => matches!(entry_command, "config" | "build" | "common" | "always"),
        "cquery" => matches!(entry_command, "cquery" | "build" | "common" | "always"),
        "aquery" => matches!(entry_command, "aquery" | "build" | "common" | "always"),
        other => matches!(entry_command, "common" | "always") || entry_command == other,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::BazelrcPoliciesCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    #[tokio::test]
    async fn required_flag_passes_when_declared_in_changed_file() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join(".bazelrc"),
            "build --downloader_config=/etc/bazel/downloader.cfg\n",
        )
        .expect("write bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "required_flag", commands = ["build"], flag = "downloader_config", value = "/etc/bazel/downloader.cfg" }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn required_flag_passes_when_satisfied_by_imported_file() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("tools/bazel")).expect("create dirs");
        fs::write(
            temp.path().join(".bazelrc"),
            "import %workspace%/tools/bazel/ci.bazelrc\n",
        )
        .expect("write bazelrc");
        fs::write(
            temp.path().join("tools/bazel/ci.bazelrc"),
            "build --downloader_config=/etc/bazel/downloader.cfg\n",
        )
        .expect("write imported bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "required_flag", commands = ["build"], flag = "downloader_config", value = "/etc/bazel/downloader.cfg" }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn required_flag_respects_command_inheritance() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join(".bazelrc"),
            "build --downloader_config=/etc/bazel/downloader.cfg\n",
        )
        .expect("write bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "required_flag", commands = ["test"], flag = "downloader_config", value = "/etc/bazel/downloader.cfg" }]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn required_flag_reports_missing_declaration() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join(".bazelrc"), "build --jobs=200\n").expect("write bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "required_flag", commands = ["build"], flag = "downloader_config", value = "/etc/bazel/downloader.cfg" }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0]
                .location
                .as_ref()
                .and_then(|loc| loc.line),
            None
        );
        assert!(result.findings[0].message.contains("downloader_config"));
    }

    #[tokio::test]
    async fn forbidden_flag_reports_changed_entry() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join(".bazelrc"),
            "common --remote_download_all\nbuild --jobs=200\n",
        )
        .expect("write bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "forbidden_flag", commands = ["build"], flag = "remote_download_all" }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0]
                .location
                .as_ref()
                .and_then(|loc| loc.line),
            Some(1)
        );
        assert!(result.findings[0].message.contains("remote_download_all"));
    }

    #[tokio::test]
    async fn ignores_config_scoped_entries_for_unconditional_rules() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join(".bazelrc"),
            "build:ci --downloader_config=/etc/bazel/downloader.cfg\n",
        )
        .expect("write bazelrc");

        let check = BazelrcPoliciesCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".bazelrc").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    rules = [{ kind = "required_flag", commands = ["build"], flag = "downloader_config", value = "/etc/bazel/downloader.cfg" }]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }
}
