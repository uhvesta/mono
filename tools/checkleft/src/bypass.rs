use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::input::ChangeSet;
use crate::output::{Finding, Location, Severity};

/// Parse BYPASS directives from free-form text.
///
/// Accepted format per line:
///   BYPASS_NAME=Reason text
pub fn parse_bypass_directives(text: &str) -> BTreeMap<String, String> {
    let mut directives = BTreeMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("BYPASS_") {
            continue;
        }

        let Some((raw_name, raw_reason)) = trimmed.split_once('=') else {
            continue;
        };

        let name = normalize_bypass_name(raw_name);
        if !is_valid_bypass_name(&name) {
            continue;
        }

        let reason = raw_reason.trim();
        if reason.is_empty() {
            continue;
        }

        directives.insert(name, reason.to_owned());
    }

    directives
}

pub fn parse_bypass_directives_from_descriptions(
    commit_description: Option<&str>,
    pr_description: Option<&str>,
) -> BTreeMap<String, String> {
    let mut directives = BTreeMap::new();

    if let Some(commit_description) = commit_description {
        directives.extend(parse_bypass_directives(commit_description));
    }
    if let Some(pr_description) = pr_description {
        // PR description overrides commit description when both specify the same bypass.
        directives.extend(parse_bypass_directives(pr_description));
    }

    directives
}

pub fn bypass_name_for_check_id(check_id: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_underscore = false;

    for ch in check_id.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_uppercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            normalized.push('_');
            last_was_underscore = true;
        }
    }

    while normalized.ends_with('_') {
        normalized.pop();
    }
    while normalized.starts_with('_') {
        normalized.remove(0);
    }

    format!("BYPASS_{normalized}")
}

pub fn bypass_failure_guidance(bypass_name: &str) -> String {
    format!(
        "Request a one-off PR exception using `{bypass_name}=<specific legitimate reason>` in the PR or commit description. Only for a real exception or emergency - never use bypasses for convenience."
    )
}

pub fn bypass_applied_finding(bypass_name: &str, reason: &str, location: Option<Location>) -> Finding {
    Finding {
        severity: Severity::Warning,
        message: format!("check was bypassed via `{bypass_name}`"),
        location,
        remediations: vec![format!(
            "Bypass reason: {reason}. Keep bypasses rare and only for legitimate exceptions."
        )],
        suggested_fix: None,
    }
}

pub fn maybe_bypass_findings(
    changeset: &ChangeSet,
    allow_bypass: bool,
    bypass_name: &str,
    trigger_files: &[PathBuf],
) -> Option<Vec<Finding>> {
    if !allow_bypass {
        return None;
    }

    let reason = changeset.bypass_reason(bypass_name)?;
    let location = trigger_files.first().cloned().map(|path| Location {
        path,
        line: None,
        column: None,
    });

    Some(vec![bypass_applied_finding(bypass_name, &reason, location)])
}

fn normalize_bypass_name(raw_name: &str) -> String {
    raw_name.trim().to_ascii_uppercase()
}

fn is_valid_bypass_name(name: &str) -> bool {
    name.starts_with("BYPASS_")
        && name.len() > "BYPASS_".len()
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch == '_' || ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{
        bypass_applied_finding, bypass_failure_guidance, bypass_name_for_check_id, maybe_bypass_findings,
        parse_bypass_directives, parse_bypass_directives_from_descriptions,
    };
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::{Location, Severity};
    use std::path::PathBuf;

    #[test]
    fn parses_bypass_directives_from_freeform_text() {
        let parsed = parse_bypass_directives(
            r#"
                some markdown text
                BYPASS_API_BREAKING_SURFACE=This does not change public API behavior.
                BYPASS_FRONTEND_NO_LEGACY_API = Legacy import remains in test fixture only.
            "#,
        );

        assert_eq!(
            parsed.get("BYPASS_API_BREAKING_SURFACE"),
            Some(&"This does not change public API behavior.".to_owned())
        );
        assert_eq!(
            parsed.get("BYPASS_FRONTEND_NO_LEGACY_API"),
            Some(&"Legacy import remains in test fixture only.".to_owned())
        );
    }

    #[test]
    fn ignores_invalid_or_empty_bypass_directives() {
        let parsed = parse_bypass_directives(
            r#"
                BYPASS_API_BREAKING_SURFACE=
                BYPASS API_BREAKING_SURFACE=Bad name
                BYPASS_MISSING_EQUALS
            "#,
        );

        assert!(parsed.is_empty());
    }

    #[test]
    fn maps_check_id_to_expected_bypass_name() {
        assert_eq!(
            bypass_name_for_check_id("api-breaking-surface"),
            "BYPASS_API_BREAKING_SURFACE"
        );
        assert_eq!(
            bypass_name_for_check_id("frontend-no-legacy-api"),
            "BYPASS_FRONTEND_NO_LEGACY_API"
        );
    }

    #[test]
    fn maps_namespaced_check_id_to_expected_bypass_name() {
        // Slashes and hyphens both become underscores; result is uppercased.
        assert_eq!(bypass_name_for_check_id("format/rust"), "BYPASS_FORMAT_RUST");
        assert_eq!(bypass_name_for_check_id("format/bazel"), "BYPASS_FORMAT_BAZEL");
        assert_eq!(bypass_name_for_check_id("lint/rust"), "BYPASS_LINT_RUST");
        assert_eq!(bypass_name_for_check_id("lint/bazel"), "BYPASS_LINT_BAZEL");
        assert_eq!(
            bypass_name_for_check_id("rust/giant-structs"),
            "BYPASS_RUST_GIANT_STRUCTS"
        );
    }

    #[test]
    fn pr_description_overrides_commit_description() {
        let parsed = parse_bypass_directives_from_descriptions(
            Some("BYPASS_API_BREAKING_SURFACE=From commit"),
            Some("BYPASS_API_BREAKING_SURFACE=From pr"),
        );

        assert_eq!(parsed.get("BYPASS_API_BREAKING_SURFACE"), Some(&"From pr".to_owned()));
    }

    #[test]
    fn bypass_failure_guidance_includes_strict_wording() {
        let guidance = bypass_failure_guidance("BYPASS_API_BREAKING_SURFACE");
        assert!(guidance.contains("BYPASS_API_BREAKING_SURFACE=<specific legitimate reason>"));
        assert!(guidance.contains("never use bypasses for convenience"));
    }

    #[test]
    fn bypass_applied_finding_is_warning_with_reason() {
        let finding = bypass_applied_finding(
            "BYPASS_API_BREAKING_SURFACE",
            "No public API surface changed.",
            Some(Location {
                path: PathBuf::from("backend/blob/src/v3/auth.rs"),
                line: None,
                column: None,
            }),
        );

        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.contains("BYPASS_API_BREAKING_SURFACE"));
        assert!(
            finding
                .remediations
                .iter()
                .any(|r| r.contains("No public API surface changed."))
        );
    }

    #[test]
    fn maybe_bypass_findings_returns_warning_finding_when_enabled_and_present() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("backend/blob/src/v3/auth.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API surface changed.".to_owned(),
        ));
        let trigger_files = vec![PathBuf::from("backend/blob/src/v3/auth.rs")];

        let findings = maybe_bypass_findings(&changeset, true, "BYPASS_API_BREAKING_SURFACE", &trigger_files)
            .expect("expected bypass findings");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn maybe_bypass_findings_is_none_when_disabled() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("backend/blob/src/v3/auth.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API surface changed.".to_owned(),
        ));
        let trigger_files = vec![PathBuf::from("backend/blob/src/v3/auth.rs")];

        assert!(maybe_bypass_findings(&changeset, false, "BYPASS_API_BREAKING_SURFACE", &trigger_files).is_none());
    }
}
