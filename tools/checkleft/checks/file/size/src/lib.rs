//! Checkleft check: flag files exceeding a configured line-count limit.
//!
//! Registered under the canonical id `file/size`. Runs inside the checkleft
//! wasm host and reads files via the WASI filesystem sandbox.
//!
//! ## What the check detects
//!
//! Any changed file that (a) is not deleted, (b) exceeds `max_lines`, and (c)
//! actually grew in the current change is flagged with a warning finding.
//!
//! File exclusion (`exclude` / `exclude_files` / `exclude_globs`) is enforced by the
//! framework host, which subtracts excluded paths from the changeset before it is
//! lowered into this check — so an excluded file never reaches the loop below.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "max_lines": 500
//! }
//! ```

use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput, Finding, check};
use serde::Deserialize;

const DEFAULT_MAX_LINES: usize = 500;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    max_lines: Option<u64>,
}

#[check(
    name = "file/size",
    description = "flags files exceeding configured line limits",
    severity = warning
)]
pub fn file_size_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let max_lines = cfg.max_lines.map(|v| v as usize).unwrap_or(DEFAULT_MAX_LINES);

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        if file.kind == ChangeKind::Deleted {
            continue;
        }

        let content = match std::fs::read_to_string(&file.path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let line_count = content.lines().count();
        if line_count <= max_lines {
            continue;
        }

        if !file_grew_in_change(file, &input.changeset) {
            continue;
        }

        let growth_message = growth_message_for_file(&file.path, &input.changeset);

        findings.push(
            Finding::warning(format!(
                "file has {line_count} lines, exceeding configured max_lines={max_lines}.{growth_message}"
            ))
            .at_column(&file.path, (max_lines.saturating_add(1)) as u32, 1)
            .with_remediation("Split the file or refactor into smaller modules to reduce line count.".to_owned()),
        );
    }

    findings
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check and
// `rust/giant-structs` into a single multiplexed component. That dedups the
// shared wasm runtime baseline (std/alloc/SDK/wit-bindgen/serde) across the
// preinstalled checks instead of duplicating it per component.

fn file_grew_in_change(file: &ChangedFile, changeset: &ChangeSet) -> bool {
    if file.kind == ChangeKind::Added {
        return true;
    }
    let Some(diff) = changeset.file_diffs.iter().find(|d| d.path == file.path) else {
        return false;
    };
    let added: u32 = diff.hunks.iter().map(|h| h.added_lines).sum();
    let removed: u32 = diff.hunks.iter().map(|h| h.removed_lines).sum();
    added > removed
}

fn growth_message_for_file(path: &str, changeset: &ChangeSet) -> String {
    let Some(diff) = changeset.file_diffs.iter().find(|d| d.path == path) else {
        return String::new();
    };
    let added: u32 = diff.hunks.iter().map(|h| h.added_lines).sum();
    let removed: u32 = diff.hunks.iter().map(|h| h.removed_lines).sum();
    format!(" File grew by +{added} / -{removed} lines in this change.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Serialize CWD changes so parallel tests don't interfere.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn make_changeset(path: &str, kind: ChangeKind, added_lines: u32, removed_lines: u32) -> ChangeSet {
        let diffs = if added_lines > 0 || removed_lines > 0 {
            vec![FileDiff {
                path: path.to_owned(),
                hunks: vec![DiffHunk {
                    old_start: 0,
                    old_lines: removed_lines,
                    new_start: 1,
                    new_lines: added_lines,
                    added_lines,
                    removed_lines,
                }],
            }]
        } else {
            vec![]
        };
        ChangeSet {
            changed_files: vec![ChangedFile {
                path: path.to_owned(),
                kind,
                old_path: None,
            }],
            file_diffs: diffs,
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
            base_files: vec![],
        }
    }

    fn run_with_config(changeset: ChangeSet, config_json: &str) -> Vec<Finding> {
        let input = CheckInput::__from_parts(changeset, config_json.to_owned());
        file_size_check(input)
    }

    #[test]
    fn flags_files_over_limit() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join("big.rs"), "a\nb\nc\n").unwrap();

        let findings = run_with_config(
            make_changeset("big.rs", ChangeKind::Modified, 2, 0),
            r#"{"max_lines": 2}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("max_lines=2"),
            "message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn ignores_files_within_limit() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join("small.rs"), "a\nb\n").unwrap();

        let findings = run_with_config(
            make_changeset("small.rs", ChangeKind::Modified, 1, 0),
            r#"{"max_lines": 5}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty());
    }

    #[test]
    fn ignores_oversized_file_when_net_lines_do_not_increase() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join("big.rs"), "a\nb\nc\n").unwrap();

        // Net change: +1 / -2 → file shrank on net
        let findings = run_with_config(
            make_changeset("big.rs", ChangeKind::Modified, 1, 2),
            r#"{"max_lines": 2}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty());
    }

    #[test]
    fn newly_added_file_over_limit_is_flagged() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join("new_big.rs"), "a\nb\nc\n").unwrap();

        // Added files always count as "grew"
        let findings = run_with_config(
            make_changeset("new_big.rs", ChangeKind::Added, 3, 0),
            r#"{"max_lines": 2}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn finding_message_includes_line_growth() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join("big.rs"), "a\nb\nc\n").unwrap();

        let findings = run_with_config(
            make_changeset("big.rs", ChangeKind::Modified, 2, 0),
            r#"{"max_lines": 2}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("File grew by +2 / -0"),
            "message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn deleted_files_are_never_flagged() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        // No file on disk: a deleted oversized file must be skipped before any read.
        let findings = run_with_config(
            make_changeset("gone.rs", ChangeKind::Deleted, 0, 10),
            r#"{"max_lines": 2}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty());
    }

    #[test]
    fn default_max_lines_applies_when_unset() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        // 501 lines: just over the built-in default of 500, with no max_lines in config.
        let body = "x\n".repeat(501);
        fs::write(dir.path().join("big.rs"), &body).unwrap();

        let findings = run_with_config(make_changeset("big.rs", ChangeKind::Added, 501, 0), "{}");

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("max_lines=500"),
            "message was: {}",
            findings[0].message
        );
    }
}
