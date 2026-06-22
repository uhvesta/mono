//! Trivial example checkleft check component.
//!
//! This check demonstrates the SDK ergonomics: declare a function with
//! `#[check]`, implement the check logic using standard Rust, then call
//! `export_checks!` once to wire up the component ABI.
//!
//! The first check only looks at the modified files — the default
//! `modified-only` access scope — and reports a warning for any file whose
//! path contains the string "do_not_submit".
//!
//! The second check demonstrates the **fix** ergonomics: a `#[check(... fix =
//! fn)]` whose fixer returns `Vec<FileEdit>`, exercising the W1 host-applied-edits
//! path end-to-end through the SDK macro and the component ABI.

use checkleft_check_sdk::{CheckInput, FileEdit, Finding, check, export_checks};
use serde::Deserialize;

/// Optional per-check configuration. Keys match the CHECKS.yaml config block
/// for this check.
#[derive(Deserialize, Default)]
struct Config {
    /// Extra patterns to flag beyond the built-in "do_not_submit" marker.
    #[serde(default)]
    extra_patterns: Vec<String>,
}

/// A check that flags files whose paths contain "do_not_submit" (or any extra
/// patterns configured by the repository).
///
/// No `access_scope` is specified, so the sandbox contains only the files
/// modified in the current changeset (the `modified-only` default).
#[check(name = "trivial-do-not-submit")]
fn trivial_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();

    let mut patterns = vec!["do_not_submit".to_owned()];
    patterns.extend(cfg.extra_patterns);

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        for pattern in &patterns {
            if file.path.contains(pattern.as_str()) {
                findings.push(
                    Finding::error(format!("File path contains `{}` marker: {}", pattern, file.path))
                        .with_remediation("Remove or rename the file before submitting."),
                );
            }
        }
    }

    findings
}

/// A check that flags changed files containing trailing whitespace, with an
/// auto-fix that strips it.
///
/// Demonstrates a content-based check + fix: the check reads each modified file
/// from the read-only sandbox (`std::fs`) and flags trailing spaces/tabs; the
/// `fix` companion returns a whole-file [`FileEdit`] with the stripped content.
/// The host validates the edit targets a fixable file and copies it back.
#[check(name = "trivial-trailing-whitespace", severity = warning, fix = fix_trailing_whitespace)]
fn check_trailing_whitespace(input: CheckInput) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in &input.changeset.changed_files {
        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        if has_trailing_whitespace(&content) {
            findings.push(Finding::warning(format!("Trailing whitespace in {}", file.path)));
        }
    }
    findings
}

/// Fixer for `trivial-trailing-whitespace`: strip trailing spaces/tabs from each
/// line of every flagged file, emitting one whole-file [`FileEdit`] per file that
/// actually changes.
fn fix_trailing_whitespace(input: CheckInput) -> Vec<FileEdit> {
    let mut edits = Vec::new();
    for file in &input.changeset.changed_files {
        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        let stripped = strip_trailing_whitespace(&content);
        if stripped != content {
            edits.push(FileEdit {
                path: file.path.clone(),
                old_text: content,
                new_text: stripped,
            });
        }
    }
    edits
}

/// True when any line in `content` ends in a space or tab.
fn has_trailing_whitespace(content: &str) -> bool {
    content.lines().any(|line| line.ends_with(' ') || line.ends_with('\t'))
}

/// Strip trailing spaces/tabs from every line, preserving a final newline.
fn strip_trailing_whitespace(content: &str) -> String {
    let mut out = content
        .lines()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

export_checks!(trivial_check, check_trailing_whitespace);
