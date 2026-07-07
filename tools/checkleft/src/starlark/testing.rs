use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;
use walkdir::WalkDir;

use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree, TreeVersion};
use crate::output::{CheckResult, FileEdit, Finding, Severity};
use crate::path::validate_relative_path;
use crate::starlark::discovery::{DiscoveredCheck, discover_package_checks};
use crate::starlark::{StarlarkCheckRunner, StarlarkCheckSource};

#[derive(Debug, Clone, Default)]
pub struct StarlarkTestOptions {
    pub selector: Option<String>,
    pub update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarlarkTestSuiteResult {
    pub cases: Vec<StarlarkTestCaseResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarlarkTestCaseResult {
    pub check_id: String,
    pub case_name: String,
    pub passed: bool,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct ExpectedOutput {
    #[serde(default)]
    findings: Vec<ExpectedFinding>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct ExpectedFinding {
    severity: ExpectedSeverity,
    #[serde(default)]
    message_contains: Option<String>,
    #[serde(default)]
    message_eq: Option<String>,
    path: PathBuf,
    #[serde(default)]
    line: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExpectedSeverity {
    Fail,
    FailButOverridable,
}

pub fn run_package_tests(
    repo_root: &Path,
    checkleft_root: &Path,
    options: &StarlarkTestOptions,
) -> Result<StarlarkTestSuiteResult> {
    validate_relative_path(checkleft_root)?;
    let package_tree = FixturePackageTree {
        repo_root: repo_root.to_path_buf(),
        before_root: PathBuf::new(),
        after_root: PathBuf::new(),
        checkleft_root: checkleft_root.to_path_buf(),
    };
    let checks = discover_package_checks(&package_tree, checkleft_root)?;
    let selected = checks
        .into_iter()
        .filter(|check| selector_matches_check(options.selector.as_deref(), check))
        .collect::<Vec<_>>();

    let mut cases = Vec::new();
    for check in selected {
        let testdata = check.check_dir.join("testdata");
        let abs_testdata = repo_root.join(&testdata);
        if !abs_testdata.exists() {
            continue;
        }
        for case_dir in test_case_dirs(&abs_testdata)? {
            let case_name = case_dir
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("non-UTF-8 test case path {}", case_dir.display()))?
                .to_owned();
            if !selector_matches(options.selector.as_deref(), &check, Some(&case_name)) {
                continue;
            }
            cases.push(run_test_case(
                repo_root,
                checkleft_root,
                &check,
                &case_dir,
                &case_name,
                options,
            )?);
        }
    }
    cases.sort_by(|left, right| {
        left.check_id
            .cmp(&right.check_id)
            .then_with(|| left.case_name.cmp(&right.case_name))
    });
    Ok(StarlarkTestSuiteResult { cases })
}

fn run_test_case(
    repo_root: &Path,
    checkleft_root: &Path,
    check: &DiscoveredCheck,
    case_dir: &Path,
    case_name: &str,
    options: &StarlarkTestOptions,
) -> Result<StarlarkTestCaseResult> {
    let before_root = case_dir.join("before");
    let after_root = case_dir.join("after");
    let expected_path = case_dir.join("expected.toml");
    if !before_root.exists() {
        bail!("{} is missing before/", case_dir.display());
    }
    if !after_root.exists() {
        bail!("{} is missing after/", case_dir.display());
    }
    let changeset = fixture_changeset(&before_root, &after_root)?;
    let tree = FixturePackageTree {
        repo_root: repo_root.to_path_buf(),
        before_root,
        after_root,
        checkleft_root: checkleft_root.to_path_buf(),
    };
    let source = String::from_utf8(
        tree.read_file(&check.check_path)
            .with_context(|| format!("failed to read {}", check.check_path.display()))?,
    )
    .with_context(|| format!("{} is not valid UTF-8", check.check_path.display()))?;
    let runner = StarlarkCheckRunner::new(
        StarlarkCheckSource::file(check.id.clone(), check.check_path.clone(), source)
            .with_load_context(check.checkleft_root.clone(), check.check_dir.clone()),
    );
    let actual = runner.evaluate_adapter(&check.adapter, &changeset, &tree)?;

    if options.update {
        write_expected(&expected_path, &actual)?;
        return match compare_expected_fix(repo_root, check, &runner, &changeset, &tree, &actual, case_dir) {
            Ok(()) => Ok(StarlarkTestCaseResult {
                check_id: check.id.clone(),
                case_name: case_name.to_owned(),
                passed: true,
                message: None,
            }),
            Err(err) => Ok(StarlarkTestCaseResult {
                check_id: check.id.clone(),
                case_name: case_name.to_owned(),
                passed: false,
                message: Some(err.to_string()),
            }),
        };
    }

    let expected = parse_expected(&expected_path)?;
    match compare_findings(&expected.findings, &actual)
        .and_then(|()| compare_expected_fix(repo_root, check, &runner, &changeset, &tree, &actual, case_dir))
    {
        Ok(()) => Ok(StarlarkTestCaseResult {
            check_id: check.id.clone(),
            case_name: case_name.to_owned(),
            passed: true,
            message: None,
        }),
        Err(err) => Ok(StarlarkTestCaseResult {
            check_id: check.id.clone(),
            case_name: case_name.to_owned(),
            passed: false,
            message: Some(err.to_string()),
        }),
    }
}

fn compare_expected_fix(
    repo_root: &Path,
    check: &DiscoveredCheck,
    runner: &StarlarkCheckRunner,
    changeset: &ChangeSet,
    tree: &FixturePackageTree,
    actual: &CheckResult,
    case_dir: &Path,
) -> Result<()> {
    let expected_fix_root = case_dir.join("expected_fix");
    if !expected_fix_root.exists() {
        return Ok(());
    }
    let fix_path = check
        .fix_path
        .as_ref()
        .ok_or_else(|| anyhow!("{} has expected_fix/ but no fix.checkleft", case_dir.display()))?;
    let fix_source = String::from_utf8(
        tree.read_file(fix_path)
            .with_context(|| format!("failed to read {}", fix_path.display()))?,
    )
    .with_context(|| format!("{} is not valid UTF-8", fix_path.display()))?;
    let fix_source = StarlarkCheckSource::file(check.id.clone(), fix_path.clone(), fix_source)
        .with_load_context(check.checkleft_root.clone(), check.check_dir.clone());
    let edits = runner.evaluate_fix_adapter(&check.adapter, fix_source, changeset, &actual.findings, tree)?;

    let fixed = tempdir().context("failed to create fixed fixture tempdir")?;
    copy_tree_contents(&tree.after_root, fixed.path())?;
    apply_file_edits(fixed.path(), &edits)?;
    compare_trees(&expected_fix_root, fixed.path()).with_context(|| {
        format!(
            "fixed output for {} did not match {}",
            check.id,
            path_relative_to(repo_root, &expected_fix_root).display()
        )
    })
}

fn parse_expected(path: &Path) -> Result<ExpectedOutput> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    if text.trim().is_empty() {
        return Ok(ExpectedOutput::default());
    }
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_expected(path: &Path, actual: &CheckResult) -> Result<()> {
    let expected = ExpectedOutput {
        findings: actual
            .findings
            .iter()
            .enumerate()
            .map(|(index, finding)| expected_finding_from_actual(index, finding))
            .collect::<Result<Vec<_>>>()?,
    };
    let mut text = toml::to_string_pretty(&expected).context("failed to serialize expected findings")?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn expected_finding_from_actual(index: usize, finding: &Finding) -> Result<ExpectedFinding> {
    let severity = match finding.severity {
        Severity::Error => ExpectedSeverity::Fail,
        Severity::Warning => ExpectedSeverity::FailButOverridable,
        Severity::Info => bail!("finding {index} has unsupported snapshot severity: info"),
    };
    let location = finding
        .location
        .as_ref()
        .ok_or_else(|| anyhow!("finding {index} has no location"))?;
    Ok(ExpectedFinding {
        severity,
        message_contains: None,
        message_eq: Some(finding.message.clone()),
        path: location.path.clone(),
        line: location.line,
    })
}

fn test_case_dirs(testdata: &Path) -> Result<Vec<PathBuf>> {
    let mut cases = Vec::new();
    for entry in fs::read_dir(testdata).with_context(|| format!("failed to list {}", testdata.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("expected.toml").exists() {
            cases.push(path);
        }
    }
    cases.sort();
    Ok(cases)
}

fn selector_matches(selector: Option<&str>, check: &DiscoveredCheck, case_name: Option<&str>) -> bool {
    let Some(selector) = selector else {
        return true;
    };
    selector == check.id
        || case_name
            .map(|case_name| selector == format!("{}/{}", check.id, case_name))
            .unwrap_or(false)
}

fn selector_matches_check(selector: Option<&str>, check: &DiscoveredCheck) -> bool {
    let Some(selector) = selector else {
        return true;
    };
    selector == check.id
        || selector
            .strip_prefix(&check.id)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn fixture_changeset(before_root: &Path, after_root: &Path) -> Result<ChangeSet> {
    let before = fixture_files(before_root)?;
    let after = fixture_files(after_root)?;
    let all = before.union(&after).cloned().collect::<BTreeSet<_>>();
    let changed_files = all
        .into_iter()
        .filter_map(
            |path| match fixture_change_kind(before_root, after_root, &before, &after, &path) {
                Ok(Some(kind)) => Some(Ok(ChangedFile {
                    path,
                    kind,
                    old_path: None,
                })),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            },
        )
        .collect::<Result<Vec<_>>>()?;
    Ok(ChangeSet::new(changed_files))
}

fn fixture_change_kind(
    before_root: &Path,
    after_root: &Path,
    before: &BTreeSet<PathBuf>,
    after: &BTreeSet<PathBuf>,
    path: &Path,
) -> Result<Option<ChangeKind>> {
    let in_before = before.contains(path);
    let in_after = after.contains(path);
    Ok(match (in_before, in_after) {
        (true, true) => {
            let before_bytes = fs::read(before_root.join(path))
                .with_context(|| format!("failed to read before fixture {}", path.display()))?;
            let after_bytes = fs::read(after_root.join(path))
                .with_context(|| format!("failed to read after fixture {}", path.display()))?;
            if before_bytes == after_bytes {
                return Ok(None);
            }
            Some(ChangeKind::Modified)
        }
        (false, true) => Some(ChangeKind::Added),
        (true, false) => Some(ChangeKind::Deleted),
        (false, false) => None,
    })
}

fn fixture_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    for entry in WalkDir::new(root).follow_links(true).into_iter() {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("{} is not under {}", entry.path().display(), root.display()))?;
        files.insert(relative.to_path_buf());
    }
    Ok(files)
}

fn copy_tree_contents(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in WalkDir::new(from).follow_links(true).into_iter() {
        let entry = entry.with_context(|| format!("failed to walk {}", from.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(from)
            .with_context(|| format!("{} is not under {}", entry.path().display(), from.display()))?;
        let destination = to.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(entry.path(), &destination)
            .with_context(|| format!("failed to copy {} to {}", entry.path().display(), destination.display()))?;
    }
    Ok(())
}

fn apply_file_edits(root: &Path, edits: &[FileEdit]) -> Result<()> {
    for edit in edits {
        validate_relative_path(&edit.path).with_context(|| format!("invalid edit path {}", edit.path.display()))?;
        let path = root.join(&edit.path);
        let content =
            fs::read_to_string(&path).with_context(|| format!("failed to read fixed file {}", path.display()))?;
        let new_content = content.replacen(&edit.old_text, &edit.new_text, 1);
        fs::write(&path, new_content).with_context(|| format!("failed to write fixed file {}", path.display()))?;
    }
    Ok(())
}

fn compare_trees(expected_root: &Path, actual_root: &Path) -> Result<()> {
    let expected_files = fixture_files(expected_root)?;
    let actual_files = fixture_files(actual_root)?;
    if expected_files != actual_files {
        bail!(
            "fixed file set mismatch: expected {:?}, got {:?}",
            expected_files,
            actual_files
        );
    }
    for path in expected_files {
        let expected = fs::read(expected_root.join(&path))
            .with_context(|| format!("failed to read expected fixed file {}", path.display()))?;
        let actual = fs::read(actual_root.join(&path))
            .with_context(|| format!("failed to read actual fixed file {}", path.display()))?;
        if expected != actual {
            bail!("fixed file mismatch at {}", path.display());
        }
    }
    Ok(())
}

fn path_relative_to<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

fn compare_findings(expected: &[ExpectedFinding], actual: &CheckResult) -> Result<()> {
    if expected.len() != actual.findings.len() {
        bail!(
            "expected {} findings, got {}: {:?}",
            expected.len(),
            actual.findings.len(),
            actual.findings
        );
    }
    for (idx, (expected, actual)) in expected.iter().zip(actual.findings.iter()).enumerate() {
        compare_finding(idx, expected, actual)?;
    }
    Ok(())
}

fn compare_finding(index: usize, expected: &ExpectedFinding, actual: &Finding) -> Result<()> {
    let expected_severity = match expected.severity {
        ExpectedSeverity::Fail => Severity::Error,
        ExpectedSeverity::FailButOverridable => Severity::Warning,
    };
    if actual.severity != expected_severity {
        bail!(
            "finding {index} severity mismatch: expected {:?}, got {:?}",
            expected_severity,
            actual.severity
        );
    }
    let location = actual
        .location
        .as_ref()
        .ok_or_else(|| anyhow!("finding {index} has no location"))?;
    if location.path != expected.path {
        bail!(
            "finding {index} path mismatch: expected {}, got {}",
            expected.path.display(),
            location.path.display()
        );
    }
    if let Some(line) = expected.line
        && location.line != Some(line)
    {
        bail!(
            "finding {index} line mismatch: expected {:?}, got {:?}",
            Some(line),
            location.line
        );
    }
    if let Some(message_eq) = &expected.message_eq
        && actual.message != *message_eq
    {
        bail!(
            "finding {index} message mismatch: expected {:?}, got {:?}",
            message_eq,
            actual.message
        );
    }
    if let Some(message_contains) = &expected.message_contains
        && !actual.message.contains(message_contains)
    {
        bail!(
            "finding {index} message mismatch: expected message containing {:?}, got {:?}",
            message_contains,
            actual.message
        );
    }
    Ok(())
}

struct FixturePackageTree {
    repo_root: PathBuf,
    before_root: PathBuf,
    after_root: PathBuf,
    checkleft_root: PathBuf,
}

impl FixturePackageTree {
    fn repo_path(&self, path: &Path) -> PathBuf {
        self.repo_root.join(path)
    }
}

impl SourceTree for FixturePackageTree {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        validate_relative_path(path)?;
        if path.starts_with(&self.checkleft_root) {
            return fs::read(self.repo_path(path)).with_context(|| format!("failed to read {}", path.display()));
        }
        fs::read(self.after_root.join(path)).with_context(|| format!("failed to read fixture file {}", path.display()))
    }

    fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>> {
        validate_relative_path(path)?;
        if path.starts_with(&self.checkleft_root) {
            return self.read_file(path);
        }
        let root = match version {
            TreeVersion::Current => &self.after_root,
            TreeVersion::Base => &self.before_root,
        };
        fs::read(root.join(path)).with_context(|| format!("failed to read fixture file {}", path.display()))
    }

    fn exists(&self, path: &Path) -> bool {
        if validate_relative_path(path).is_err() {
            return false;
        }
        if path.starts_with(&self.checkleft_root) {
            return self.repo_path(path).exists();
        }
        self.after_root.join(path).exists()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        validate_relative_path(path)?;
        let directory = self.repo_path(path);
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory).with_context(|| format!("failed to list {}", path.display()))? {
            let entry = entry?;
            let absolute = entry.path();
            let relative = absolute
                .strip_prefix(&self.repo_root)
                .with_context(|| format!("{} is not under {}", absolute.display(), self.repo_root.display()))?;
            entries.push(relative.to_path_buf());
        }
        entries.sort();
        Ok(entries)
    }

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let glob = globset::Glob::new(pattern)?.compile_matcher();
        Ok(fixture_files(&self.after_root)?
            .into_iter()
            .filter(|path| glob.is_match(path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn runs_text_check_fixture_test_case() {
        let temp = tempdir().expect("create temp dir");
        write_package_fixture(temp.path());

        let result = run_package_tests(
            temp.path(),
            Path::new("checkleft"),
            &StarlarkTestOptions {
                selector: Some("text/no_debug/debug_added".to_owned()),
                update: false,
            },
        )
        .expect("run tests");

        assert_eq!(result.cases.len(), 1);
        assert!(result.cases[0].passed, "{:?}", result.cases[0].message);
        assert_eq!(result.cases[0].check_id, "text/no_debug");
        assert_eq!(result.cases[0].case_name, "debug_added");
    }

    #[test]
    fn reports_expected_finding_mismatch() {
        let temp = tempdir().expect("create temp dir");
        write_package_fixture(temp.path());
        fs::write(
            temp.path()
                .join("checkleft/text/no_debug/testdata/debug_added/expected.toml"),
            r#"
[[findings]]
severity = "fail"
message_contains = "different"
path = "notes/example.txt"
"#,
        )
        .expect("rewrite expected");

        let result =
            run_package_tests(temp.path(), Path::new("checkleft"), &StarlarkTestOptions::default()).expect("run tests");

        assert_eq!(result.cases.len(), 1);
        assert!(!result.cases[0].passed);
        assert!(
            result.cases[0]
                .message
                .as_ref()
                .expect("message")
                .contains("message mismatch")
        );
    }

    #[test]
    fn updates_expected_finding_snapshot() {
        let temp = tempdir().expect("create temp dir");
        write_package_fixture(temp.path());
        let expected_path = temp
            .path()
            .join("checkleft/text/no_debug/testdata/debug_added/expected.toml");
        fs::write(&expected_path, "").expect("clear expected");

        let result = run_package_tests(
            temp.path(),
            Path::new("checkleft"),
            &StarlarkTestOptions {
                selector: Some("text/no_debug/debug_added".to_owned()),
                update: true,
            },
        )
        .expect("run tests");

        assert_eq!(result.cases.len(), 1);
        assert!(result.cases[0].passed, "{:?}", result.cases[0].message);
        let updated = fs::read_to_string(expected_path).expect("read expected");
        assert!(updated.contains("message_eq = \"debug text added\""), "{updated}");
        assert!(updated.contains("path = \"notes/example.txt\""), "{updated}");
        assert!(updated.contains("line = 2"), "{updated}");
    }

    fn write_package_fixture(root: &Path) {
        fs::create_dir_all(root.join("checkleft/lib")).expect("create lib");
        fs::create_dir_all(root.join("checkleft/text/no_debug/testdata/debug_added/before/notes"))
            .expect("create before");
        fs::create_dir_all(root.join("checkleft/text/no_debug/testdata/debug_added/after/notes"))
            .expect("create after");
        fs::write(
            root.join("checkleft/package.toml"),
            r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
        )
        .expect("write manifest");
        fs::write(
            root.join("checkleft/lib/messages.checkleft"),
            r#"
def message_for(kind):
    return kind + " text added"
"#,
        )
        .expect("write lib");
        fs::write(
            root.join("checkleft/text/no_debug/check.checkleft"),
            r#"
load("//lib/messages", "message_for")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if "debug" in line.text:
                findings.append(fail(
                    message = message_for("debug"),
                    path = file.path,
                    line = line.number,
                    column = 1,
                ))
    return findings
"#,
        )
        .expect("write check");
        fs::write(
            root.join("checkleft/text/no_debug/testdata/debug_added/before/notes/example.txt"),
            "hello\n",
        )
        .expect("write before");
        fs::write(
            root.join("checkleft/text/no_debug/testdata/debug_added/after/notes/example.txt"),
            "hello\ndebug mode\n",
        )
        .expect("write after");
        fs::write(
            root.join("checkleft/text/no_debug/testdata/debug_added/expected.toml"),
            r#"
[[findings]]
severity = "fail"
message_contains = "debug text"
path = "notes/example.txt"
line = 2
"#,
        )
        .expect("write expected");
    }
}
