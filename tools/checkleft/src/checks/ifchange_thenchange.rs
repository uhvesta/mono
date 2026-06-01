use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};
use crate::ifchange::{
    LineRange, ParsedIfChangeBlock, ParsedIfChangeFile, ThenChangeTarget, parse_ifchange_file,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, FileDiff, SourceTree, TreeVersion};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct IfChangeThenChangeCheck;

#[async_trait]
impl Check for IfChangeThenChangeCheck {
    fn id(&self) -> &str {
        "ifchange-thenchange"
    }

    fn description(&self) -> &str {
        "requires linked files or blocks to change together"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for IfChangeThenChangeCheck {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let analyses: Vec<_> = changeset
            .changed_files
            .iter()
            .map(|changed_file| analyze_changed_file(changed_file, changeset, tree))
            .collect();

        let mut findings = Vec::new();
        let mut emitted_keys = BTreeSet::new();

        for analysis in &analyses {
            findings.extend(analysis.parse_findings.clone());
        }

        for analysis in &analyses {
            if !analysis.parse_findings.is_empty() {
                continue;
            }

            for block in analysis.contracts_to_check() {
                let key = format!(
                    "{}:{}:{}:{:?}",
                    analysis.path.display(),
                    block.ifchange_line,
                    block.thenchange_line,
                    block.target
                );
                if !emitted_keys.insert(key) {
                    continue;
                }

                findings.push(match target_status(block, changeset, tree, &analyses)? {
                    TargetStatus::Satisfied => continue,
                    TargetStatus::MissingFile => broken_target_finding(
                        analysis.path.clone(),
                        block,
                        "linked target file does not exist in the current tree".to_owned(),
                    ),
                    TargetStatus::MissingLabel => broken_target_finding(
                        analysis.path.clone(),
                        block,
                        "linked target label does not exist in the current tree".to_owned(),
                    ),
                    TargetStatus::NotChanged => broken_target_finding(
                        analysis.path.clone(),
                        block,
                        "linked target was not updated in the same change".to_owned(),
                    ),
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
struct ChangedFileAnalysis {
    path: PathBuf,
    current_touched: Vec<ParsedIfChangeBlock>,
    fallback_base_touched: Vec<ParsedIfChangeBlock>,
    parse_findings: Vec<Finding>,
}

impl ChangedFileAnalysis {
    fn contracts_to_check(&self) -> impl Iterator<Item = &ParsedIfChangeBlock> {
        self.current_touched
            .iter()
            .chain(self.fallback_base_touched.iter())
    }
}

fn analyze_changed_file(
    changed_file: &ChangedFile,
    changeset: &ChangeSet,
    tree: &dyn SourceTree,
) -> ChangedFileAnalysis {
    let mut parse_findings = Vec::new();
    let Some(diff) = changeset.file_diffs.get(&changed_file.path) else {
        return ChangedFileAnalysis {
            path: changed_file.path.clone(),
            current_touched: Vec::new(),
            fallback_base_touched: Vec::new(),
            parse_findings,
        };
    };

    let current = if !matches!(changed_file.kind, ChangeKind::Deleted) {
        match parse_versioned_file(
            tree,
            &changed_file.path,
            TreeVersion::Current,
            &changed_file.path,
            Severity::Error,
        ) {
            Ok(parsed) => Some(parsed),
            Err(finding) => {
                parse_findings.push(finding);
                None
            }
        }
    } else {
        None
    };

    let previous_path = previous_path_for_changed_file(changed_file);
    let base = if let Some(previous_path) = previous_path.as_ref() {
        match parse_versioned_file(
            tree,
            previous_path,
            TreeVersion::Base,
            &changed_file.path,
            Severity::Error,
        ) {
            Ok(parsed) => Some(parsed),
            Err(finding) => {
                parse_findings.push(finding);
                None
            }
        }
    } else {
        None
    };

    let current_touched = current
        .as_ref()
        .map(|parsed| touched_blocks_new(parsed, diff))
        .unwrap_or_default();
    let fallback_base_touched = if current_touched.is_empty() {
        base.as_ref()
            .map(|parsed| touched_blocks_old(parsed, diff))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    ChangedFileAnalysis {
        path: changed_file.path.clone(),
        current_touched,
        fallback_base_touched,
        parse_findings,
    }
}

fn previous_path_for_changed_file(changed_file: &ChangedFile) -> Option<PathBuf> {
    if matches!(changed_file.kind, ChangeKind::Added) {
        return None;
    }

    Some(
        changed_file
            .old_path
            .clone()
            .unwrap_or_else(|| changed_file.path.clone()),
    )
}

fn parse_versioned_file(
    tree: &dyn SourceTree,
    path: &Path,
    version: TreeVersion,
    finding_path: &Path,
    severity: Severity,
) -> std::result::Result<ParsedIfChangeFile, Finding> {
    let contents = tree
        .read_file_versioned(path, version)
        .map_err(|error| Finding {
            severity,
            message: format!(
                "failed to read `{}` for ifchange analysis: {error}",
                path.display()
            ),
            location: Some(Location {
                path: finding_path.to_path_buf(),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: None,
        })?;
    let contents = String::from_utf8(contents).map_err(|error| Finding {
        severity,
        message: format!(
            "failed to parse `{}` for ifchange analysis as utf-8: {error}",
            path.display()
        ),
        location: Some(Location {
            path: finding_path.to_path_buf(),
            line: None,
            column: None,
        }),
        remediations: vec![],
        suggested_fix: None,
    })?;

    parse_ifchange_file(path, &contents).map_err(|error| Finding {
        severity,
        message: error.to_string(),
        location: Some(Location {
            path: finding_path.to_path_buf(),
            line: None,
            column: None,
        }),
        remediations: vec![],
        suggested_fix: None,
    })
}

fn touched_blocks_new(parsed: &ParsedIfChangeFile, diff: &FileDiff) -> Vec<ParsedIfChangeBlock> {
    parsed
        .blocks
        .iter()
        .filter(|block| diff_touches_range_new(diff, block.full_range()))
        .cloned()
        .collect()
}

fn touched_blocks_old(parsed: &ParsedIfChangeFile, diff: &FileDiff) -> Vec<ParsedIfChangeBlock> {
    parsed
        .blocks
        .iter()
        .filter(|block| diff_touches_range_old(diff, block.full_range()))
        .cloned()
        .collect()
}

fn diff_touches_range_new(diff: &FileDiff, range: LineRange) -> bool {
    diff.hunks
        .iter()
        .any(|hunk| hunk_touches_range(hunk.new_start, hunk.new_lines, range))
}

fn diff_touches_range_old(diff: &FileDiff, range: LineRange) -> bool {
    diff.hunks
        .iter()
        .any(|hunk| hunk_touches_range(hunk.old_start, hunk.old_lines, range))
}

fn hunk_touches_range(start: usize, len: usize, range: LineRange) -> bool {
    if len == 0 {
        return start >= range.start && start <= range.end.saturating_add(1);
    }

    let end = start.saturating_add(len.saturating_sub(1));
    start <= range.end && end >= range.start
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetStatus {
    Satisfied,
    MissingFile,
    MissingLabel,
    NotChanged,
}

fn target_status(
    block: &ParsedIfChangeBlock,
    changeset: &ChangeSet,
    tree: &dyn SourceTree,
    analyses: &[ChangedFileAnalysis],
) -> Result<TargetStatus> {
    match &block.target {
        ThenChangeTarget::File { path } => {
            if !tree.exists(path) {
                return Ok(TargetStatus::MissingFile);
            }
            Ok(if file_changed(changeset, path) {
                TargetStatus::Satisfied
            } else {
                TargetStatus::NotChanged
            })
        }
        ThenChangeTarget::Block { path, label } => {
            if !tree.exists(path) {
                return Ok(TargetStatus::MissingFile);
            }

            let parsed_current =
                parse_ifchange_file(path, &String::from_utf8(tree.read_file(path)?)?)?;
            if parsed_current.block_by_label(label).is_none() {
                return Ok(TargetStatus::MissingLabel);
            }

            let Some(target_analysis) = analyses.iter().find(|analysis| analysis.path == *path)
            else {
                return Ok(TargetStatus::NotChanged);
            };
            Ok(
                if target_analysis
                    .current_touched
                    .iter()
                    .any(|candidate| candidate.source_label.as_deref() == Some(label))
                    || target_analysis
                        .fallback_base_touched
                        .iter()
                        .any(|candidate| candidate.source_label.as_deref() == Some(label))
                {
                    TargetStatus::Satisfied
                } else {
                    TargetStatus::NotChanged
                },
            )
        }
    }
}

fn file_changed(changeset: &ChangeSet, target_path: &Path) -> bool {
    changeset.changed_files.iter().any(|changed_file| {
        changed_file.path == target_path
            || changed_file
                .old_path
                .as_ref()
                .is_some_and(|old_path| old_path == target_path)
    })
}

fn broken_target_finding(
    source_path: PathBuf,
    block: &ParsedIfChangeBlock,
    detail: String,
) -> Finding {
    Finding {
        severity: Severity::Error,
        message: format!("{}: {detail}", render_target(&block.target)),
        location: Some(Location {
            path: source_path,
            line: Some(block.ifchange_line as u32),
            column: Some(1),
        }),
        remediations: vec![
            "Update the linked file or block in the same change, or bypass the check with a documented reason."
                .to_owned(),
        ],
        suggested_fix: None,
    }
}

fn render_target(target: &ThenChangeTarget) -> String {
    match target {
        ThenChangeTarget::File { path } => format!("`LINT.ThenChange({})`", path.display()),
        ThenChangeTarget::Block { path, label } => {
            format!("`LINT.ThenChange({}:{label})`", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::tempdir;

    use super::IfChangeThenChangeCheck;
    use crate::check::Check;
    use crate::output::{CheckResult, Severity};
    use crate::source_tree::LocalSourceTree;
    use crate::vcs::{BaseRevision, Vcs};

    #[tokio::test]
    async fn passes_when_source_and_target_change_together() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());
        write_linked_pair(temp.path());
        commit_all(temp.path(), "initial");

        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .expect("write source");
        fs::write(temp.path().join("frontend/schema.txt"), "schema view v2\n")
            .expect("write target");

        let result = run_check(temp.path()).await;
        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn fails_when_linked_target_file_does_not_change() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());
        write_linked_pair(temp.path());
        commit_all(temp.path(), "initial");

        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .expect("write source");

        let result = run_check(temp.path()).await;
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        assert!(result.findings[0].message.contains("frontend/schema.txt"));
    }

    #[tokio::test]
    async fn passes_when_linked_target_block_changes() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());
        fs::create_dir_all(temp.path().join("backend")).expect("mkdir backend");
        fs::create_dir_all(temp.path().join("frontend")).expect("mkdir frontend");
        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=1\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .expect("write source");
        fs::write(
            temp.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=1\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .expect("write target");
        commit_all(temp.path(), "initial");

        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .expect("write source");
        fs::write(
            temp.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=2\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .expect("write target");

        let result = run_check(temp.path()).await;
        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn reports_missing_target_label() {
        let temp = tempdir().expect("tempdir");
        init_git_repo(temp.path());
        fs::create_dir_all(temp.path().join("backend")).expect("mkdir backend");
        fs::create_dir_all(temp.path().join("frontend")).expect("mkdir frontend");
        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=1\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .expect("write source");
        fs::write(temp.path().join("frontend/schema.txt"), "render=1\n").expect("write target");
        commit_all(temp.path(), "initial");

        fs::write(
            temp.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .expect("write source");

        let result = run_check(temp.path()).await;
        assert_eq!(result.findings.len(), 1);
        assert!(
            result.findings[0]
                .message
                .contains("frontend/schema.txt:view")
        );
    }

    fn write_linked_pair(root: &Path) {
        fs::create_dir_all(root.join("backend")).expect("mkdir backend");
        fs::create_dir_all(root.join("frontend")).expect("mkdir frontend");
        fs::write(
            root.join("backend/schema.txt"),
            "// LINT.IfChange\nschema v1\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .expect("write source");
        fs::write(root.join("frontend/schema.txt"), "schema view v1\n").expect("write target");
    }

    async fn run_check(root: &Path) -> CheckResult {
        let vcs = Vcs::detect(root).expect("detect vcs");
        let changeset = vcs.current_changeset().expect("current changeset");
        let tree =
            LocalSourceTree::with_base_revision(root, Some(BaseRevision::Git("HEAD".to_owned())))
                .expect("tree");

        IfChangeThenChangeCheck
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::map::Map::new()),
            )
            .await
            .expect("run check")
    }

    fn init_git_repo(root: &Path) {
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "checkleft@example.com"]);
        run_git(root, &["config", "user.name", "Checkleft"]);
    }

    fn commit_all(root: &Path, message: &str) {
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", message]);
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
