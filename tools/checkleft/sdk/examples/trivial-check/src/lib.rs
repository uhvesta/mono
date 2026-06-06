//! Trivial example checkleft check component.
//!
//! This check demonstrates the SDK ergonomics: declare a function with
//! `#[check]`, implement the check logic using standard Rust, then call
//! `export_checks!` once to wire up the component ABI.
//!
//! The check itself only looks at the modified files — the default
//! `modified-only` access scope — and reports a warning for any file whose
//! path contains the string "do_not_submit".

use checkleft_check_sdk::{check, export_checks, CheckInput, Finding};
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
                    Finding::error(format!(
                        "File path contains `{}` marker: {}",
                        pattern, file.path
                    ))
                    .with_remediation("Remove or rename the file before submitting."),
                );
            }
        }
    }

    findings
}

export_checks!(trivial_check);
