//! Checkleft check: flag changed files whose paths match forbidden glob patterns.
//!
//! Registered under the canonical id `file/forbidden-path`. Runs inside the
//! checkleft wasm host and receives changeset data via the WIT contract.
//!
//! ## What the check detects
//!
//! Any changed file that (a) matches at least one rule's `patterns` glob and (b)
//! has a change kind listed in that rule's `when` is flagged with an error finding
//! carrying the rule's remediation message.
//!
//! File exclusion (`exclude` / `exclude_files` / `exclude_globs`) is enforced by the
//! framework host, which subtracts excluded paths from the changeset before it is
//! lowered into this check — so an excluded file never reaches the loop below.
//!
//! For renamed files both the new path and the old path are candidates; the first
//! matching candidate determines the finding's location.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "rules": [
//!     {
//!       "remediation": "Generated artifacts must not be committed.",
//!       "when": ["added", "modified", "renamed"],
//!       "patterns": ["**/target/**", "**/node_modules/**"]
//!     }
//!   ],
//!   "severity": "error"
//! }
//! ```
//!
//! `severity`: optional override (`"error"`, `"warning"`, or `"info"`). Defaults
//! to `"error"`.

use checkleft_check_sdk::{ChangeKind, ChangedFile, CheckInput, Finding, Location, Severity, check};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    rules: Vec<RuleConfig>,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Deserialize)]
struct RuleConfig {
    remediation: String,
    #[serde(default)]
    when: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
}

struct CompiledRule {
    remediation: String,
    when: Vec<ChangeKind>,
    pattern_strings: Vec<String>,
    patterns: GlobSet,
}

#[check(
    name = "file/forbidden-path",
    description = "flags changed files whose paths match forbidden glob patterns",
    severity = error
)]
pub fn forbidden_path_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = match input.config() {
        Ok(c) => c,
        Err(e) => {
            return vec![Finding::error(format!("invalid file/forbidden-path check config: {e}"))];
        }
    };

    if cfg.rules.is_empty() {
        return vec![Finding::error(
            "invalid file/forbidden-path check config: must contain at least one `rules` entry",
        )];
    }

    let severity = parse_severity(cfg.severity.as_deref());

    let rules = match compile_rules(&cfg.rules) {
        Ok(r) => r,
        Err(e) => {
            return vec![Finding::error(format!("invalid file/forbidden-path check config: {e}"))];
        }
    };

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        for rule in &rules {
            if !rule.when.contains(&file.kind) {
                continue;
            }

            let candidates = candidate_paths(file);
            for candidate in &candidates {
                let matches = rule.patterns.matches(candidate.as_str());
                if matches.is_empty() {
                    continue;
                }

                let matched_pattern = &rule.pattern_strings[matches[0]];
                let kind_name = change_kind_name(file.kind);

                let mut finding = Finding::error(format!(
                    "path `{}` is forbidden for {kind_name} changes. (matched `{matched_pattern}`)",
                    candidate,
                ));
                finding.severity = severity;
                finding.location = Some(Location {
                    path: candidate.clone(),
                    line: None,
                    column: None,
                });
                finding.remediations.push(rule.remediation.clone());
                findings.push(finding);
                break;
            }
        }
    }

    findings
}

fn candidate_paths(file: &ChangedFile) -> Vec<String> {
    let mut paths = vec![file.path.clone()];
    if file.kind == ChangeKind::Renamed
        && let Some(old_path) = &file.old_path
        && *old_path != file.path
    {
        paths.push(old_path.clone());
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

fn parse_severity(s: Option<&str>) -> Severity {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("warning") | Some("warn") => Severity::Warning,
        Some("info") => Severity::Info,
        _ => Severity::Error,
    }
}

fn compile_rules(rules: &[RuleConfig]) -> Result<Vec<CompiledRule>, String> {
    let mut compiled = Vec::with_capacity(rules.len());
    for (index, rule) in rules.iter().enumerate() {
        let field_prefix = format!("rules[{index}]");
        if rule.remediation.trim().is_empty() {
            return Err(format!("`{field_prefix}.remediation` must not be empty"));
        }
        if rule.when.is_empty() {
            return Err(format!("`{field_prefix}.when` must contain at least one change kind"));
        }
        if rule.patterns.is_empty() {
            return Err(format!("`{field_prefix}.patterns` must contain at least one pattern"));
        }

        let when = rule
            .when
            .iter()
            .map(|s| parse_change_kind(s))
            .collect::<Result<Vec<_>, _>>()?;

        let patterns = build_globset(&rule.patterns).map_err(|e| format!("{e} in `{field_prefix}.patterns`"))?;

        compiled.push(CompiledRule {
            remediation: rule.remediation.clone(),
            when,
            pattern_strings: rule.patterns.clone(),
            patterns,
        });
    }
    Ok(compiled)
}

fn parse_change_kind(s: &str) -> Result<ChangeKind, String> {
    match s {
        "added" => Ok(ChangeKind::Added),
        "modified" => Ok(ChangeKind::Modified),
        "deleted" => Ok(ChangeKind::Deleted),
        "renamed" => Ok(ChangeKind::Renamed),
        other => Err(format!(
            "unknown change kind `{other}`; expected one of: added, modified, deleted, renamed"
        )),
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| format!("invalid glob `{pattern}`: {e}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput};

    fn make_input(changed_files: Vec<ChangedFile>, config_json: &str) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files,
                file_diffs: vec![],
                commit_description: None,
                pr_description: None,
                change_id: None,
                repository: None,
                base_files: vec![],
            },
            config_json.to_owned(),
        )
    }

    fn run(changed_files: Vec<ChangedFile>, config_json: &str) -> Vec<Finding> {
        let input = make_input(changed_files, config_json);
        forbidden_path_check(input)
    }

    #[test]
    fn flags_added_path_for_matching_rule() {
        let findings = run(
            vec![ChangedFile {
                path: "mobile/ios/.build/workspace-state.json".to_owned(),
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"Generated artifacts must not be committed. Remove them from the change.","when":["added"],"patterns":["**/.build/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert_eq!(
            findings[0].remediations.first().map(String::as_str),
            Some("Generated artifacts must not be committed. Remove them from the change.")
        );
        assert!(findings[0].message.contains("**/.build/**"));
    }

    #[test]
    fn does_not_flag_added_file_for_modified_only_rule() {
        let findings = run(
            vec![ChangedFile {
                path: "mobile/ios/.build/workspace-state.json".to_owned(),
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"Generated artifacts must not be edited.","when":["modified"],"patterns":["**/.build/**"]}]}"#,
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn flags_deleted_files_when_delete_rule_matches() {
        let findings = run(
            vec![ChangedFile {
                path: "backend/legacy/config.toml".to_owned(),
                kind: ChangeKind::Deleted,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"Compatibility config must not be removed.","when":["deleted"],"patterns":["backend/legacy/config.toml"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].remediations.first().map(String::as_str),
            Some("Compatibility config must not be removed.")
        );
        assert_eq!(
            findings[0].location.as_ref().map(|l| l.path.as_str()),
            Some("backend/legacy/config.toml")
        );
    }

    #[test]
    fn flags_renamed_files_when_new_path_matches() {
        let findings = run(
            vec![ChangedFile {
                path: "frontend/dist/app.js".to_owned(),
                kind: ChangeKind::Renamed,
                old_path: Some("frontend/src/app.js".to_owned()),
            }],
            r#"{"rules":[{"remediation":"Distribution assets must not be committed.","when":["renamed"],"patterns":["**/dist/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].location.as_ref().map(|l| l.path.as_str()),
            Some("frontend/dist/app.js")
        );
    }

    #[test]
    fn flags_renamed_files_when_old_path_matches() {
        let findings = run(
            vec![ChangedFile {
                path: "frontend/src/app.js".to_owned(),
                kind: ChangeKind::Renamed,
                old_path: Some("frontend/dist/app.js".to_owned()),
            }],
            r#"{"rules":[{"remediation":"Distribution assets must not be renamed into tracked source paths.","when":["renamed"],"patterns":["**/dist/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].location.as_ref().map(|l| l.path.as_str()),
            Some("frontend/dist/app.js")
        );
    }

    #[test]
    fn emits_one_finding_per_matching_rule() {
        let findings = run(
            vec![ChangedFile {
                path: "frontend/dist/app.js.swp".to_owned(),
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"Distribution assets must not be committed.","when":["added"],"patterns":["**/dist/**","**/build/**"]},{"remediation":"Editor scratch files do not belong in the repo.","when":["added","modified"],"patterns":["**/*.swp","**/*~"]}]}"#,
        );
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn emits_one_finding_when_multiple_patterns_match_same_rule() {
        let findings = run(
            vec![ChangedFile {
                path: "frontend/dist/app.js".to_owned(),
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"Generated outputs must not be checked in.","when":["added"],"patterns":["frontend/**","**/dist/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn requires_at_least_one_rule() {
        let findings = run(
            vec![ChangedFile {
                path: "backend/src/lib.rs".to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("rules"));
    }

    #[test]
    fn rejects_empty_rule_remediation() {
        let findings = run(
            vec![ChangedFile {
                path: "backend/src/lib.rs".to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"   ","when":["modified"],"patterns":["backend/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("remediation"));
    }

    #[test]
    fn rejects_empty_when_list() {
        let findings = run(
            vec![ChangedFile {
                path: "backend/src/lib.rs".to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"No edits allowed.","when":[],"patterns":["backend/**"]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("when"));
    }

    #[test]
    fn rejects_empty_patterns_list() {
        let findings = run(
            vec![ChangedFile {
                path: "backend/src/lib.rs".to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"No edits allowed.","when":["modified"],"patterns":[]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("patterns"));
    }

    #[test]
    fn warning_severity_from_config() {
        let findings = run(
            vec![ChangedFile {
                path: "mobile/ios/.build/foo".to_owned(),
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"No build artifacts.","when":["added"],"patterns":["**/.build/**"]}],"severity":"warning"}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn rejects_invalid_glob_pattern() {
        let findings = run(
            vec![ChangedFile {
                path: "some/file.rs".to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"rules":[{"remediation":"No edits.","when":["modified"],"patterns":["["]}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("invalid file/forbidden-path check config"));
    }

    #[test]
    fn severity_parsing_is_case_insensitive() {
        for severity_str in &["Warning", "WARNING", "warn", "WARN"] {
            let findings = run(
                vec![ChangedFile {
                    path: "mobile/ios/.build/foo".to_owned(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }],
                &format!(
                    r#"{{"rules":[{{"remediation":"No build artifacts.","when":["added"],"patterns":["**/.build/**"]}}],"severity":"{}"}}"#,
                    severity_str
                ),
            );
            assert_eq!(findings.len(), 1, "severity={}", severity_str);
            assert_eq!(findings[0].severity, Severity::Warning, "severity={}", severity_str);
        }
    }
}
