//! Checkleft check: validate internal markdown links in changed markdown files.
//!
//! This is the Component Model wasm port of the former built-in
//! `docs-link-integrity` check, registered under the canonical id
//! `md/link-integrity`. It runs inside the checkleft wasm host and reads the
//! repository via the WASI filesystem sandbox.
//!
//! ## What the check detects
//!
//! For every changed (non-deleted) markdown file, the check scans for
//! `[text](target)` links and flags any internal target whose resolved path
//! does not exist in the repository.  External links (`http://`, `https://`,
//! `mailto:`, `tel:`) and same-page anchor links (starting with `#`) are
//! silently ignored.  Image links (`![alt](target)`) are also skipped.
//!
//! ## Access scope
//!
//! The check declares `access_scope = whole_repo` so the host mounts every
//! file in the repository into the WASI sandbox.  This is necessary to verify
//! that link targets outside the changeset exist.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use checkleft_check_sdk::{ChangeKind, CheckInput, Finding, check};

#[check(
    name = "md/link-integrity",
    description = "validates internal markdown links in changed markdown files",
    severity = warning,
    access_scope = whole_repo
)]
pub fn md_link_integrity_check(input: CheckInput) -> Vec<Finding> {
    let mut findings = Vec::new();

    let changeset_paths: HashSet<&str> = input
        .changeset
        .changed_files
        .iter()
        .filter(|f| f.kind != ChangeKind::Deleted)
        .map(|f| f.path.as_str())
        .collect();

    for changed_file in &input.changeset.changed_files {
        if changed_file.kind == ChangeKind::Deleted {
            continue;
        }
        if !is_markdown_file(&changed_file.path) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&changed_file.path) else {
            continue;
        };

        for (line_index, line) in contents.lines().enumerate() {
            for (col, target) in find_links(line) {
                if should_skip_link_target(target) {
                    continue;
                }
                if link_target_exists(&changed_file.path, target, &changeset_paths) {
                    continue;
                }

                findings.push(
                    Finding::warning(format!("broken internal markdown link target `{target}`"))
                        .at_column(&changed_file.path, (line_index + 1) as u32, col as u32)
                        .with_remediation("Fix or remove the broken link target in this markdown file.".to_owned()),
                );
            }
        }
    }

    findings
}

fn is_markdown_file(path: &str) -> bool {
    Path::new(path).extension().and_then(|ext| ext.to_str()) == Some("md")
}

fn should_skip_link_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with('#')
}

fn link_target_exists(current_file: &str, target: &str, changeset_paths: &HashSet<&str>) -> bool {
    let path_part = target.split_once('#').map(|(path, _)| path).unwrap_or(target).trim();
    if path_part.is_empty() {
        return true;
    }

    let resolved = if path_part.starts_with('/') {
        normalize_path(Path::new(path_part.trim_start_matches('/')))
    } else {
        let parent = Path::new(current_file).parent().unwrap_or_else(|| Path::new(""));
        normalize_path(&parent.join(path_part))
    };

    Path::new(&resolved).exists() || changeset_paths.contains(resolved.to_str().unwrap_or(""))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Scan `line` for `[text](url)` patterns and return `(column_1indexed, url)`
/// for each non-image link found.
fn find_links(line: &str) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let bracket_pos = i;
        let is_image = bracket_pos > 0 && bytes[bracket_pos - 1] == b'!';

        // Find the matching ] (non-nested, matching original regex semantics)
        i += 1;
        while i < bytes.len() && bytes[i] != b']' {
            i += 1;
        }

        // Expect ]( immediately after
        if i + 1 < bytes.len() && bytes[i] == b']' && bytes[i + 1] == b'(' {
            i += 2;
            let url_start = i;
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
            if i < bytes.len() {
                let url = line[url_start..i].trim();
                i += 1; // skip )
                if !is_image {
                    result.push((bracket_pos + 1, url)); // 1-indexed column at the [
                }
                continue;
            }
        }
        i += 1;
    }
    result
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check
// into the single multiplexed component.

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn make_changeset(files: &[(&str, ChangeKind)]) -> ChangeSet {
        ChangeSet {
            changed_files: files
                .iter()
                .map(|(path, kind)| ChangedFile {
                    path: path.to_string(),
                    kind: *kind,
                    old_path: None,
                })
                .collect(),
            file_diffs: vec![],
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
            base_files: vec![],
        }
    }

    fn run_check(changeset: ChangeSet) -> Vec<Finding> {
        let input = CheckInput::__from_parts(changeset, "{}".to_owned());
        md_link_integrity_check(input)
    }

    #[test]
    fn flags_missing_relative_doc_link() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/index.md"), "[Missing](missing.md)\n").unwrap();

        let findings = run_check(make_changeset(&[("docs/index.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("missing.md"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn accepts_existing_relative_doc_link() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/guide.md"), "guide\n").unwrap();
        fs::write(dir.path().join("docs/index.md"), "[Guide](guide.md)\n").unwrap();

        let findings = run_check(make_changeset(&[("docs/index.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty());
    }

    #[test]
    fn accepts_link_to_file_added_in_same_changeset() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("docs/design-docs")).unwrap();
        // index.md links to new-feature.md; that file is only in the changeset
        // (not on disk), simulating adding a new doc and a link in the same commit.
        fs::write(
            dir.path().join("docs/design-docs/index.md"),
            "[New Feature](new-feature.md)\n",
        )
        .unwrap();

        let findings = run_check(make_changeset(&[
            ("docs/design-docs/index.md", ChangeKind::Modified),
            ("docs/design-docs/new-feature.md", ChangeKind::Added),
        ]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(
            findings.is_empty(),
            "link to file added in same changeset must not be flagged"
        );
    }

    #[test]
    fn checks_markdown_files_outside_docs_directory() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("guides")).unwrap();
        fs::write(dir.path().join("guides/setup.md"), "[Missing](missing.md)\n").unwrap();

        let findings = run_check(make_changeset(&[("guides/setup.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn skips_external_links() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("README.md"),
            "[Docs](https://example.com/docs)\n[Mail](mailto:user@example.com)\n[Tel](tel:+1234)\n",
        )
        .unwrap();

        let findings = run_check(make_changeset(&[("README.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty(), "external links must be skipped");
    }

    #[test]
    fn skips_anchor_only_links() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("README.md"), "[Section](#section-name)\n").unwrap();

        let findings = run_check(make_changeset(&[("README.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty(), "anchor-only links must be skipped");
    }

    #[test]
    fn skips_image_links() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("README.md"), "![Logo](logo.png)\n").unwrap();

        let findings = run_check(make_changeset(&[("README.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        // image link to non-existent logo.png must not be flagged
        assert!(findings.is_empty(), "image links must be skipped");
    }

    #[test]
    fn skips_deleted_files() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // File is deleted — no file written on disk.
        let findings = run_check(make_changeset(&[("docs/gone.md", ChangeKind::Deleted)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty(), "deleted files must be skipped");
    }

    #[test]
    fn finding_reports_correct_line_and_column() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("README.md"), "prefix [Bad](missing.md) suffix\n").unwrap();

        let findings = run_check(make_changeset(&[("README.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(findings.len(), 1);
        let loc = findings[0].location.as_ref().expect("must have location");
        assert_eq!(loc.line, Some(1));
        // "prefix " is 7 chars; "[" is at position 8 (1-indexed)
        assert_eq!(loc.column, Some(8));
    }

    #[test]
    fn resolves_parent_dir_references() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("docs/sub")).unwrap();
        fs::write(dir.path().join("docs/target.md"), "target\n").unwrap();
        fs::write(dir.path().join("docs/sub/page.md"), "[Link](../target.md)\n").unwrap();

        let findings = run_check(make_changeset(&[("docs/sub/page.md", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty(), "../target.md should resolve correctly");
    }

    #[test]
    fn skips_non_markdown_files() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("script.sh"), "[link](missing.md)\n").unwrap();

        let findings = run_check(make_changeset(&[("script.sh", ChangeKind::Modified)]));

        std::env::set_current_dir(old_cwd).unwrap();

        assert!(findings.is_empty(), "non-markdown files must be skipped");
    }
}
