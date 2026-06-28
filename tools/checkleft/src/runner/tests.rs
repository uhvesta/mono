use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};
use tempfile::tempdir;

use crate::check::{Check, CheckRegistry, ConfiguredCheck};
use crate::checks::register_builtin_checks;
use crate::config::ConfigResolver;
use crate::exclusion::{DeclaredExclusion, ExclusionStatus};
use crate::external::{
    ExternalCheckComponentPackage, ExternalCheckExecutor, ExternalCheckImplementationRef, ExternalCheckPackage,
    ExternalCheckPackageImplementation, ExternalCheckPackageProvider, parse_external_check_package_manifest,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::source_tree::LocalSourceTree;

use super::Runner;

struct StaticExternalProvider {
    package: Option<ExternalCheckPackage>,
}

impl ExternalCheckPackageProvider for StaticExternalProvider {
    fn resolve(&self, _implementation_ref: &ExternalCheckImplementationRef) -> Result<Option<ExternalCheckPackage>> {
        Ok(self.package.clone())
    }
}

struct StaticExternalExecutor {
    result: Option<CheckResult>,
    error_message: Option<String>,
    seen_packages: Arc<Mutex<Vec<String>>>,
}

impl ExternalCheckExecutor for StaticExternalExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        _changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        _config: &toml::Value,
        _config_dir: &std::path::Path,
        _effective_severity: Option<crate::output::Severity>,
        _exclusion: &crate::exclusion_matcher::ExclusionMatcher,
    ) -> Result<CheckResult> {
        self.seen_packages
            .lock()
            .expect("lock seen packages")
            .push(package.id.clone());

        if let Some(error_message) = self.error_message.as_ref() {
            anyhow::bail!("{error_message}");
        }

        Ok(self.result.clone().unwrap_or_else(|| CheckResult {
            check_id: package.id.clone(),
            findings: Vec::new(),
        }))
    }
}

/// A mock component executor for the stale-exclusion *host orchestration* tests.
///
/// It returns a caller-supplied set of declared exclusions and a fixed
/// `ExclusionStatus`, so the runner's audit plumbing (diff-gating, CHECKS-line
/// resolution, severity application, off-mode short-circuit) can be asserted
/// without compiling or running the real wasm component. The
/// stale-vs-load-bearing *determination* itself is proven natively in the
/// giant-structs check crate; here we only verify what the host does with it.
struct MockExclusionExecutor {
    declared: Vec<DeclaredExclusion>,
    status: ExclusionStatus,
    /// Number of times `evaluate_exclusion_for_component` was invoked. Zero proves
    /// the audit was short-circuited (e.g. severity = off) before consulting the check.
    evaluate_calls: Arc<Mutex<usize>>,
}

impl ExternalCheckExecutor for MockExclusionExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        _changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        _config: &toml::Value,
        _config_dir: &std::path::Path,
        _effective_severity: Option<crate::output::Severity>,
        _exclusion: &crate::exclusion_matcher::ExclusionMatcher,
    ) -> Result<CheckResult> {
        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: Vec::new(),
        })
    }

    fn declared_exclusions_for_component(
        &self,
        _package: &ExternalCheckPackage,
        _check_name: &str,
        _config_json: &str,
        _config_dir: &std::path::Path,
    ) -> Result<Vec<DeclaredExclusion>> {
        Ok(self.declared.clone())
    }

    fn evaluate_exclusion_for_component(
        &self,
        _package: &ExternalCheckPackage,
        _check_name: &str,
        _config_json: &str,
        _exclusion: &DeclaredExclusion,
        _file_content: Option<&str>,
    ) -> Result<ExclusionStatus> {
        *self.evaluate_calls.lock().expect("lock evaluate calls") += 1;
        Ok(self.status.clone())
    }
}

#[derive(Clone)]
struct CapturingCheck {
    id: String,
    seen_files: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Check for CapturingCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "captures the input files"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for CapturingCheck {
    async fn run(&self, changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        let files: Vec<_> = changeset
            .changed_files
            .iter()
            .map(|changed| changed.path.display().to_string())
            .collect();
        self.seen_files.lock().expect("lock files").extend(files);

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: Vec::new(),
        })
    }
}

#[derive(Clone)]
struct MetadataCapturingCheck {
    id: String,
    directive_name: String,
    seen_bypass_reason: Arc<Mutex<Option<String>>>,
    seen_change_id: Arc<Mutex<Option<String>>>,
    seen_repository: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Check for MetadataCapturingCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "captures description and change metadata"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for MetadataCapturingCheck {
    async fn run(&self, changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        *self.seen_bypass_reason.lock().expect("lock bypass reason") = changeset.bypass_reason(&self.directive_name);
        *self.seen_change_id.lock().expect("lock change id") = changeset.change_id.clone();
        *self.seen_repository.lock().expect("lock repository") = changeset.repository.clone();

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: Vec::new(),
        })
    }
}

/// A check that ignores its changeset entirely and always emits an error finding
/// located on a fixed path. Used to prove the finding-location backstop drops
/// findings on excluded paths even for a check that derives a path some other way.
#[derive(Clone)]
struct EmitsFixedPathCheck {
    id: String,
    finding_path: String,
}

#[async_trait]
impl Check for EmitsFixedPathCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "always emits a finding on a fixed path"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for EmitsFixedPathCheck {
    async fn run(&self, _changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "violation on a path the check derived itself".to_owned(),
                location: Some(Location {
                    path: PathBuf::from(&self.finding_path),
                    line: Some(1),
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            }],
        })
    }
}

#[tokio::test]
async fn runner_groups_files_by_check() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "capture"
"#,
    )
    .expect("write config");

    let seen_files = Arc::new(Mutex::new(Vec::new()));
    let mut registry = CheckRegistry::new();
    registry
        .register(CapturingCheck {
            id: "capture".to_owned(),
            seen_files: Arc::clone(&seen_files),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![
            ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: Path::new("backend/src/b.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    let files = seen_files.lock().expect("lock files").clone();
    assert_eq!(
        files,
        vec!["backend/src/a.rs".to_owned(), "backend/src/b.rs".to_owned()]
    );
}

#[tokio::test]
async fn runner_executes_discovered_local_starlark_text_check() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("checkleft/text/no_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("checkleft/text/no_todo")).expect("create second check dirs");
    fs::create_dir_all(temp.path().join("checkleft/lib")).expect("create lib dir");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("checkleft/text/no_debug/check.checkleft"),
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
        temp.path().join("checkleft/text/no_todo/check.checkleft"),
        r#"
load("//lib/messages", "message_for")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if "todo" in line.text:
                findings.append(fail(
                    message = message_for("todo"),
                    path = file.path,
                    line = line.number,
                    column = 1,
                ))
    return findings
"#,
    )
    .expect("write second check");
    fs::write(
        temp.path().join("checkleft/lib/messages.checkleft"),
        r#"
def message_for(kind):
    return kind + " text added"
"#,
    )
    .expect("write lib");
    fs::write(temp.path().join("notes/example.txt"), "hello\ndebug mode\ntodo item\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_debug")
        .expect("starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "debug text added");
    assert_eq!(
        result.findings[0].location,
        Some(Location {
            path: PathBuf::from("notes/example.txt"),
            line: Some(2),
            column: Some(1),
        })
    );
    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_todo")
        .expect("second starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "todo text added");
    assert_eq!(
        result.findings[0].location,
        Some(Location {
            path: PathBuf::from("notes/example.txt"),
            line: Some(3),
            column: Some(1),
        })
    );
}

#[tokio::test]
async fn runner_executes_starlark_text_check_selected_by_checks_yaml_package() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("central/checkleft/text/no_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://central/checkleft
      version: 0.1.0
      mode: all
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("central/checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("central/checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if "debug" in line.text:
                findings.append(fail(
                    message = "debug text added",
                    path = file.path,
                    line = line.number,
                    column = 1,
                ))
    return findings
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes/example.txt"), "hello\ndebug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_debug")
        .expect("starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "debug text added");
}

#[tokio::test]
async fn runner_executes_starlark_text_check_from_local_package_archive() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("packages")).expect("create packages dir");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    let archive = starlark_package_archive();
    let archive_sha256 = sha256_hex_for_test(&archive);
    fs::write(temp.path().join("packages/checks.tar.gz"), archive).expect("write archive");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        format!(
            r#"
checkleft_packages:
  packages:
    - source: path://packages/checks.tar.gz
      version: 0.1.0
      sha256: {archive_sha256}
      mode: all
"#
        ),
    )
    .expect("write CHECKS.yaml");
    fs::write(temp.path().join("notes/example.txt"), "hello\ndebug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_debug")
        .expect("archive-backed starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "debug text added from archive");
}

#[tokio::test]
async fn runner_rejects_local_package_archive_hash_mismatch() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("packages")).expect("create packages dir");
    let archive = starlark_package_archive();
    fs::write(temp.path().join("packages/checks.tar.gz"), archive).expect("write archive");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://packages/checks.tar.gz
      version: 0.1.0
      sha256: ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
      mode: all
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(temp.path().join("notes.txt"), "debug\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| {
            result.check_id == "starlark-package"
                && result
                    .findings
                    .iter()
                    .any(|finding| finding.message.contains("sha256 mismatch"))
        }),
        "expected archive hash diagnostic, got {results:?}"
    );
}

#[tokio::test]
async fn runner_executes_only_explicitly_selected_starlark_package_checks() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("central/checkleft/text/no_debug")).expect("create first check dirs");
    fs::create_dir_all(temp.path().join("central/checkleft/text/no_todo")).expect("create second check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://central/checkleft
      version: 0.1.0
      mode: explicit

checks:
  - id: text/no_debug
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("central/checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("central/checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return [fail(
        message = "debug text added",
        path = ctx.files[0].path,
        line = 1,
        column = 1,
    )]
"#,
    )
    .expect("write selected check");
    fs::write(
        temp.path().join("central/checkleft/text/no_todo/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return [fail(
        message = "todo text added",
        path = ctx.files[0].path,
        line = 1,
        column = 1,
    )]
"#,
    )
    .expect("write unselected check");
    fs::write(temp.path().join("notes/example.txt"), "debug TODO\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| result.check_id == "text/no_debug"),
        "selected Starlark check should run: {results:?}"
    );
    assert!(
        results.iter().all(|result| result.check_id != "text/no_todo"),
        "unselected Starlark check should not run: {results:?}"
    );
    assert!(
        results.iter().all(|result| !result
            .findings
            .iter()
            .any(|finding| finding.message.contains("unknown implementation"))),
        "explicit Starlark selection must not also produce built-in missing diagnostics: {results:?}"
    );
}

#[tokio::test]
async fn runner_executes_starlark_text_check_selected_by_local_version_set() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("baseline")).expect("create version set dir");
    fs::create_dir_all(temp.path().join("central/checkleft/text/no_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  version_sets:
    - source: path://baseline
      version: 2026.06.1
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("baseline/package.toml"),
        r#"
[package]
name = "local/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.central]
source = "path://central/checkleft"
version = "0.1.0"
"#,
    )
    .expect("write version set manifest");
    fs::write(
        temp.path().join("central/checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("central/checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return [fail(
        message = "debug text added",
        path = ctx.files[0].path,
        line = 1,
        column = 1,
    )]
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes/example.txt"), "debug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_debug")
        .expect("starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "debug text added");
}

#[tokio::test]
async fn runner_executes_starlark_text_check_selected_by_local_version_set_archive() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("packages")).expect("create packages dir");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");

    let package_archive = starlark_package_archive();
    let package_sha256 = sha256_hex_for_test(&package_archive);
    fs::write(temp.path().join("packages/checks.tar.gz"), package_archive).expect("write package archive");
    let version_set_archive = starlark_version_set_archive(&package_sha256);
    let version_set_sha256 = sha256_hex_for_test(&version_set_archive);
    fs::write(temp.path().join("packages/baseline.tar.gz"), version_set_archive).expect("write version-set archive");

    fs::write(
        temp.path().join("CHECKS.yaml"),
        format!(
            r#"
checkleft_packages:
  version_sets:
    - source: path://packages/baseline.tar.gz
      version: 2026.06.1
      sha256: {version_set_sha256}
"#
        ),
    )
    .expect("write CHECKS.yaml");
    fs::write(temp.path().join("notes/example.txt"), "hello\ndebug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/no_debug")
        .expect("archive-backed version-set starlark result");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "debug text added from archive");
}

#[tokio::test]
async fn runner_rejects_version_set_selected_as_package() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("baseline")).expect("create version set dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://baseline
      version: 2026.06.1
      mode: all
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("baseline/package.toml"),
        r#"
[package]
name = "local/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.central]
source = "path://central/checkleft"
version = "0.1.0"
"#,
    )
    .expect("write version set manifest");
    fs::write(temp.path().join("notes.txt"), "debug\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| {
            result.check_id == "starlark-package"
                && result
                    .findings
                    .iter()
                    .any(|finding| finding.message.contains("kind is not `check_package`"))
        }),
        "expected package-kind diagnostic, got {results:?}"
    );
}

#[tokio::test]
async fn runner_rejects_version_set_include_that_is_another_version_set() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("baseline")).expect("create baseline dir");
    fs::create_dir_all(temp.path().join("nested")).expect("create nested dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  version_sets:
    - source: path://baseline
      version: 2026.06.1
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("baseline/package.toml"),
        r#"
[package]
name = "local/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.nested]
source = "path://nested"
version = "2026.06.1"
"#,
    )
    .expect("write baseline manifest");
    fs::write(
        temp.path().join("nested/package.toml"),
        r#"
[package]
name = "local/nested"
version = "2026.06.1"
kind = "version_set"

[includes.central]
source = "path://central/checkleft"
version = "0.1.0"
"#,
    )
    .expect("write nested manifest");
    fs::write(temp.path().join("notes.txt"), "debug\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| {
            result.check_id == "starlark-package"
                && result
                    .findings
                    .iter()
                    .any(|finding| finding.message.contains("included package kind is not `check_package`"))
        }),
        "expected version-set include diagnostic, got {results:?}"
    );
}

#[tokio::test]
async fn runner_rejects_conflicting_selected_package_refs_with_same_name() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("baseline")).expect("create baseline dir");
    fs::create_dir_all(temp.path().join("central_a/checkleft/text/no_debug")).expect("create first package");
    fs::create_dir_all(temp.path().join("central_b/checkleft/text/no_debug")).expect("create second package");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://central_a/checkleft
      version: 0.1.0
      mode: all
  version_sets:
    - source: path://baseline
      version: 2026.06.1
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("baseline/package.toml"),
        r#"
[package]
name = "local/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.central_b]
source = "path://central_b/checkleft"
version = "0.2.0"
"#,
    )
    .expect("write baseline manifest");
    for (root, version) in [("central_a/checkleft", "0.1.0"), ("central_b/checkleft", "0.2.0")] {
        fs::write(
            temp.path().join(root).join("package.toml"),
            format!(
                r#"
[package]
name = "local/checks"
version = "{version}"
"#
            ),
        )
        .expect("write package manifest");
        fs::write(
            temp.path().join(root).join("text/no_debug/check.checkleft"),
            r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return []
"#,
        )
        .expect("write check");
    }
    fs::write(temp.path().join("notes.txt"), "debug\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| {
            result.check_id == "starlark-package"
                && result
                    .findings
                    .iter()
                    .any(|finding| finding.message.contains("resolves to conflicting refs"))
        }),
        "expected duplicate package diagnostic, got {results:?}"
    );
}

#[tokio::test]
async fn runner_rejects_selected_package_version_mismatch() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("central/checkleft/text/no_debug")).expect("create package");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checkleft_packages:
  packages:
    - source: path://central/checkleft
      version: 0.2.0
      mode: all
"#,
    )
    .expect("write CHECKS.yaml");
    fs::write(
        temp.path().join("central/checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("central/checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return []
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes.txt"), "debug\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(
        results.iter().any(|result| {
            result.check_id == "starlark-package"
                && result
                    .findings
                    .iter()
                    .any(|finding| finding.message.contains("does not match selected version"))
        }),
        "expected version mismatch diagnostic, got {results:?}"
    );
}

#[tokio::test]
async fn runner_filters_discovered_starlark_checks_by_applies_to() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("checkleft/text/no_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return [fail(message = "should not run")]
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes/example.md"), "debug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(!results.iter().any(|result| result.check_id == "text/no_debug"));
}

#[tokio::test]
async fn runner_lists_discovered_local_starlark_text_check() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("checkleft/text/no_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("checkleft/text/no_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return []
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes/example.txt"), "hello\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let checks = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect("list checks");

    assert_eq!(checks, vec!["text/no_debug"]);
}

#[tokio::test]
async fn runner_preserves_starlark_finding_severity() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("checkleft/text/warn_debug")).expect("create check dirs");
    fs::create_dir_all(temp.path().join("notes")).expect("create notes dir");
    fs::write(
        temp.path().join("checkleft/package.toml"),
        r#"
[package]
name = "local/checks"
version = "0.1.0"
"#,
    )
    .expect("write package manifest");
    fs::write(
        temp.path().join("checkleft/text/warn_debug/check.checkleft"),
        r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return [fail_but_overridable(
        message = "debug text added",
        path = ctx.files[0].path,
        line = 1,
    )]
"#,
    )
    .expect("write check");
    fs::write(temp.path().join("notes/example.txt"), "debug mode\n").expect("write changed file");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|result| result.check_id == "text/warn_debug")
        .expect("starlark result");
    assert_eq!(result.findings[0].severity, Severity::Warning);
}

/// Task 4: a built-in Rust check receives the host's exclusion-filtered view of the
/// changeset — an excluded path is removed before the check ever sees it, so the
/// check is never triggered on that path.
#[tokio::test]
async fn builtin_check_never_sees_excluded_files() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/vendor")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["backend/vendor/**"]

[[checks]]
id = "capture"
"#,
    )
    .expect("write config");

    let seen_files = Arc::new(Mutex::new(Vec::new()));
    let mut registry = CheckRegistry::new();
    registry
        .register(CapturingCheck {
            id: "capture".to_owned(),
            seen_files: Arc::clone(&seen_files),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    runner
        .run_changeset(&ChangeSet::new(vec![
            ChangedFile {
                path: PathBuf::from("backend/src/a.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: PathBuf::from("backend/vendor/dep.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ]))
        .await
        .expect("run checks");

    let files = seen_files.lock().expect("lock files").clone();
    assert_eq!(
        files,
        vec!["backend/src/a.rs".to_owned()],
        "the excluded vendored file must never reach the check"
    );
}

/// Task 5: the finding-location backstop drops any finding whose path is excluded,
/// uniformly — even for a check that ignores the filtered changeset and derives the
/// path itself. The excluded path is still present in the run's full changeset (so
/// `scope_findings_to_changeset` keeps it), proving the backstop is what removes it.
#[tokio::test]
async fn backstop_drops_findings_on_excluded_path() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("vendor")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/**"]

[[checks]]
id = "emits"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(EmitsFixedPathCheck {
            id: "emits".to_owned(),
            finding_path: "vendor/excluded.rs".to_owned(),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("vendor/excluded.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let findings: Vec<_> = results.iter().flat_map(|r| r.findings.iter()).collect();
    assert!(
        findings.is_empty(),
        "the backstop must drop the finding on the excluded path; got: {findings:?}"
    );
}

/// Unit-level proof that the backstop keeps location-less (check-level) findings —
/// the same exemption that protects framework-meta findings — while dropping
/// findings located on an excluded path.
#[test]
fn drop_excluded_findings_keeps_locationless_and_drops_excluded() {
    let matcher = crate::exclusion_matcher::ExclusionMatcher::new(&["vendor/**".to_owned()]).expect("matcher");
    let mut result = CheckResult {
        check_id: "demo".to_owned(),
        findings: vec![
            Finding {
                severity: Severity::Error,
                message: "on excluded path".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("vendor/dep.rs"),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Error,
                message: "on a normal path".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("src/lib.rs"),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Error,
                message: "check-level error, no location".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            },
        ],
    };

    super::drop_excluded_findings(&mut result, &matcher);

    let messages: Vec<&str> = result.findings.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(messages, vec!["on a normal path", "check-level error, no location"]);
}

#[tokio::test]
async fn runner_propagates_description_and_change_metadata_to_checks() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "capture-descriptions"
"#,
    )
    .expect("write config");

    let directive_name = "BYPASS_CAPTURE_DESCRIPTIONS".to_owned();
    let seen_bypass_reason = Arc::new(Mutex::new(None));
    let seen_change_id = Arc::new(Mutex::new(None));
    let seen_repository = Arc::new(Mutex::new(None));
    let mut registry = CheckRegistry::new();
    registry
        .register(MetadataCapturingCheck {
            id: "capture-descriptions".to_owned(),
            directive_name: directive_name.clone(),
            seen_bypass_reason: Arc::clone(&seen_bypass_reason),
            seen_change_id: Arc::clone(&seen_change_id),
            seen_repository: Arc::clone(&seen_repository),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some(
                "BYPASS_CAPTURE_DESCRIPTIONS=Legitimate exception for validation.".to_owned(),
            ))
            .with_change_id(Some("235".to_owned()))
            .with_repository(Some("example/flunge".to_owned())),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(
        *seen_bypass_reason.lock().expect("lock bypass reason"),
        Some("Legitimate exception for validation.".to_owned())
    );
    assert_eq!(*seen_change_id.lock().expect("lock change id"), Some("235".to_owned()));
    assert_eq!(
        *seen_repository.lock().expect("lock repository"),
        Some("example/flunge".to_owned())
    );
}

#[tokio::test]
async fn runner_ignores_checks_toml_by_default() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "capture"
"#,
    )
    .expect("write config");

    let seen_files = Arc::new(Mutex::new(Vec::new()));
    let mut registry = CheckRegistry::new();
    registry
        .register(CapturingCheck {
            id: "capture".to_owned(),
            seen_files: Arc::clone(&seen_files),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("CHECKS.toml").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert!(results.is_empty());
    let files = seen_files.lock().expect("lock files").clone();
    assert!(files.is_empty());

    let configured = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("CHECKS.toml").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect("list checks");
    assert!(configured.is_empty());
}

#[tokio::test]
async fn runner_can_opt_in_to_check_checks_toml() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
include_config_files = true

[[checks]]
id = "capture"
"#,
    )
    .expect("write config");

    let seen_files = Arc::new(Mutex::new(Vec::new()));
    let mut registry = CheckRegistry::new();
    registry
        .register(CapturingCheck {
            id: "capture".to_owned(),
            seen_files: Arc::clone(&seen_files),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("CHECKS.toml").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    let files = seen_files.lock().expect("lock files").clone();
    assert_eq!(files, vec!["CHECKS.toml".to_owned()]);

    let configured = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("CHECKS.toml").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect("list checks");
    assert_eq!(configured, vec!["capture".to_owned()]);
}

#[tokio::test]
async fn runner_reports_check_errors_in_output() {
    struct FailingCheck;

    #[async_trait]
    impl Check for FailingCheck {
        fn id(&self) -> &str {
            "fails"
        }

        fn description(&self) -> &str {
            "fails intentionally"
        }

        fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
            Ok(Arc::new(Self))
        }
    }

    #[async_trait]
    impl ConfiguredCheck for FailingCheck {
        async fn run(&self, _changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
            anyhow::bail!("boom");
        }
    }

    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "fails"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry.register(FailingCheck).expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/src/a.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "fails");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("boom"));
    // Execution failures must carry source attribution — not <unknown>.
    assert!(
        results[0].findings[0].location.is_some(),
        "execution-failure finding must have a location (got <unknown>)"
    );
    assert_eq!(
        results[0].findings[0]
            .location
            .as_ref()
            .unwrap()
            .path
            .file_name()
            .and_then(|n| n.to_str()),
        Some("CHECKS.toml"),
        "execution-failure location must point at the CHECKS config file"
    );
}

#[tokio::test]
async fn runner_external_execution_failure_carries_source_attribution() {
    // Verifies that when an external check executor returns Err, the resulting
    // finding has a location pointing at the CHECKS config file — not <unknown>.
    use crate::external::{
        EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, ExternalCheckComponentPackage,
        ExternalCheckPackage, ExternalCheckPackageImplementation,
    };
    use std::path::Path;

    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: my-ext-check
    check: my-ext-check
    implementation: bundled:my-ext-check
"#,
    )
    .expect("write config");
    fs::write(temp.path().join("src/a.rs"), "fn main() {}").expect("write source");

    // Use a fake sha256 to trigger a digest-mismatch error from the executor.
    let fake_pkg = ExternalCheckPackage {
        id: "my-ext-check".to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
            artifact_path: "nonexistent.wasm".to_owned(),
            artifact_sha256: "a".repeat(64),
            artifact_bytes: None,
            check_name: "my-ext-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = Arc::new(StaticExternalExecutor {
        result: None,
        error_message: Some("simulated wasm crash".to_owned()),
        seen_packages: Arc::clone(&seen_packages),
    });
    let provider = Arc::new(StaticExternalProvider {
        package: Some(fake_pkg),
    });

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        provider,
        executor,
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("src/a.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    let result = results
        .iter()
        .find(|r| r.check_id == "my-ext-check")
        .expect("must have result for my-ext-check");

    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Error);
    assert!(
        result.findings[0].message.contains("simulated wasm crash"),
        "error message must include the underlying error; got: {}",
        result.findings[0].message
    );
    assert!(
        result.findings[0].location.is_some(),
        "execution-failure finding must have a location (got <unknown>)"
    );
    assert_eq!(
        result.findings[0]
            .location
            .as_ref()
            .unwrap()
            .path
            .file_name()
            .and_then(|n| n.to_str()),
        Some("CHECKS.yaml"),
        "execution-failure location must point at the CHECKS config file"
    );
}

#[tokio::test]
async fn runner_reports_malformed_checks_yaml_as_config_finding() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: file-size
    config:
      max_lines: [1, 2
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/src/a.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "checks-config");
    assert_eq!(
        results[0].findings[0].location.as_ref().map(|location| &location.path),
        Some(&Path::new("CHECKS.yaml").to_path_buf())
    );
    assert!(results[0].findings[0].message.contains("failed to parse checks config"));
}

#[tokio::test]
async fn runner_reports_invalid_builtin_config_on_checks_file() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "typo"

[checks.config]
rules = "not-a-list"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/src/a.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "typo");
    assert_eq!(
        results[0].findings[0].location.as_ref().map(|location| &location.path),
        Some(&Path::new("CHECKS.toml").to_path_buf())
    );
    assert!(results[0].findings[0].message.contains("invalid typo check config"));
    assert!(!results[0].findings[0].message.contains("check execution failed"));
}

#[tokio::test]
async fn runner_reports_unknown_configured_checks() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "spelling-typos"
check = "not-registered"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/src/a.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "spelling-typos");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("unknown implementation"));
}

#[tokio::test]
async fn runner_reports_instance_id_not_implementation_id() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "teh value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "spelling"
check = "capture"
"#,
    )
    .expect("write config");

    let seen_files = Arc::new(Mutex::new(Vec::new()));
    let mut registry = CheckRegistry::new();
    registry
        .register(CapturingCheck {
            id: "capture".to_owned(),
            seen_files,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "spelling");
}

/// Build a temp repo whose `tools/boss/CHECKS.toml` configures the builder check with a
/// qualified exclusion for `engine/src/app.rs::ServerState`, and writes `app.rs` with the
/// given body. `settings` is inserted verbatim above the `[[checks]]` block.
fn boss_repo_with_app(settings: &str, app_source: &str) -> tempfile::TempDir {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("tools/boss/engine/src")).expect("create dirs");
    fs::write(
        temp.path().join("tools/boss/CHECKS.toml"),
        format!(
            r#"{settings}
[[checks]]
id = "rust/giant-structs"

[checks.config]
exclude_structs = ["engine/src/app.rs::ServerState"]
"#
        ),
    )
    .expect("write config");
    fs::write(temp.path().join("tools/boss/engine/src/app.rs"), app_source).expect("write app");
    temp
}

const SERVER_STATE_WITH_BUILDER: &str = r#"
#[derive(bon::Builder)]
pub struct ServerState {
    a: String, b: String, c: String, d: String, e: String, f: String,
}
"#;

const SERVER_STATE_WITHOUT_BUILDER: &str = r#"
pub struct ServerState {
    a: String, b: String, c: String, d: String, e: String, f: String,
}
"#;

/// Return a shared executor for the four builder-audit tests.
///
/// `OnceLock::get_or_init` is blocking: the first caller initializes the
/// executor (deserializing the precompiled AOT `.cwasm` under Bazel, or
/// performing a one-time JIT+cache under plain `cargo test`) while all later
/// callers wait.  This ensures the compilation/deserialization happens at most
/// once per test-binary run, preventing the four tests from each independently
/// running it when they execute in parallel.
fn shared_builder_audit_executor() -> Arc<dyn ExternalCheckExecutor> {
    use crate::external::test_support::executor_with_precompiled_cache;
    // Keep a TempDir alive for the executor's root for the process lifetime.
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    static EXECUTOR: OnceLock<Arc<dyn ExternalCheckExecutor>> = OnceLock::new();
    let root = ROOT.get_or_init(|| {
        let t = tempdir().expect("shared builder-audit root");
        let path = t.path().to_path_buf();
        std::mem::forget(t); // keep alive for the process lifetime
        path
    });
    Arc::clone(EXECUTOR.get_or_init(|| Arc::new(executor_with_precompiled_cache(root))))
}

/// Run the audit for the standard `tools/boss/engine/src/app.rs` changeset with an
/// arbitrary external executor (real wasm component or mock).
async fn run_builder_audit_with_executor(
    temp: &tempfile::TempDir,
    executor: Arc<dyn ExternalCheckExecutor>,
) -> Vec<CheckResult> {
    use crate::external::BundledExternalCheckPackageProvider;
    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");
    // The bundled provider only resolves package metadata (cheap sha256 over the
    // embedded bytes); whether any wasm is compiled/run is up to the executor.
    let runner = Runner::with_external(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(BundledExternalCheckPackageProvider),
        executor,
    );
    runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("tools/boss/engine/src/app.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks")
}

/// The real-component parity variant: runs the audit through the actual bundled
/// wasm component (shared across the round-trip tests, see
/// [`shared_builder_audit_executor`]).
async fn run_builder_audit(temp: &tempfile::TempDir) -> Vec<CheckResult> {
    run_builder_audit_with_executor(temp, shared_builder_audit_executor()).await
}

/// Run the audit with a MOCK component executor that declares one
/// `engine/src/app.rs::ServerState` exclusion (depending on the changed file) and
/// reports `status`. Returns the results plus the mock's evaluate-call count.
async fn run_mock_exclusion_audit(temp: &tempfile::TempDir, status: ExclusionStatus) -> (Vec<CheckResult>, usize) {
    let evaluate_calls = Arc::new(Mutex::new(0usize));
    let executor = Arc::new(MockExclusionExecutor {
        declared: vec![DeclaredExclusion {
            entry: "engine/src/app.rs::ServerState".to_owned(),
            depends_on: vec![Path::new("tools/boss/engine/src/app.rs").to_path_buf()],
        }],
        status,
        evaluate_calls: Arc::clone(&evaluate_calls),
    });
    let results = run_builder_audit_with_executor(temp, executor).await;
    let calls = *evaluate_calls.lock().expect("lock evaluate calls");
    (results, calls)
}

fn stale_findings(results: &[CheckResult]) -> Vec<&Finding> {
    results
        .iter()
        .flat_map(|result| result.findings.iter())
        .filter(|finding| finding.message.contains("is no longer needed"))
        .collect()
}

/// Layer-3 parity round-trip: the REAL bundled wasm component, run end-to-end
/// through the audit, must produce a stale finding anchored on the CHECKS.toml
/// entry line. This is the one real-component exclusion-audit test kept to prove
/// the component's `declared-exclusions` / `evaluate-exclusion` hooks behave
/// identically to the native determination; the orchestration variants below use
/// a mock executor so they don't pay the wasm cost.
#[tokio::test]
async fn stale_exclusion_surfaced_on_checks_toml_when_struct_gains_builder() {
    let temp = boss_repo_with_app("", SERVER_STATE_WITH_BUILDER);
    let results = run_builder_audit(&temp).await;

    let stale = stale_findings(&results);
    assert_eq!(stale.len(), 1, "expected one stale finding, got {results:?}");
    let finding = stale[0];
    // Default severity is a warning.
    assert_eq!(finding.severity, Severity::Warning);
    // Reported on the CHECKS.toml entry, not on the changed source file.
    let location = finding.location.as_ref().expect("finding has a location");
    assert_eq!(location.path, Path::new("tools/boss/CHECKS.toml").to_path_buf());
    assert_eq!(location.line, Some(6), "should point at the exclude_structs entry line");
    assert!(finding.message.contains("engine/src/app.rs::ServerState"));
}

// Layer-2 host orchestration: the verdict is supplied by a mock executor (the
// determination itself is proven natively in the giant-structs crate); these
// assert only what the *host* does with that verdict.

#[tokio::test]
async fn load_bearing_exclusion_is_not_flagged() {
    let temp = boss_repo_with_app("", SERVER_STATE_WITHOUT_BUILDER);
    let (results, calls) = run_mock_exclusion_audit(&temp, ExclusionStatus::LoadBearing).await;
    assert_eq!(
        calls, 1,
        "the audit must consult the check exactly once for the dependent exclusion"
    );
    assert!(
        stale_findings(&results).is_empty(),
        "load-bearing exclusion must stay quiet, got {results:?}"
    );
}

#[tokio::test]
async fn stale_exclusion_severity_setting_upgrades_to_error() {
    let temp = boss_repo_with_app(
        "[settings]\nstale_exclusion_severity = \"error\"\n",
        SERVER_STATE_WITH_BUILDER,
    );
    let (results, _calls) = run_mock_exclusion_audit(
        &temp,
        ExclusionStatus::Stale {
            reason: "ServerState now satisfies the builder rule".to_owned(),
        },
    )
    .await;
    let stale = stale_findings(&results);
    assert_eq!(stale.len(), 1, "expected one stale finding, got {results:?}");
    assert_eq!(stale[0].severity, Severity::Error);
}

#[tokio::test]
async fn stale_exclusion_severity_off_disables_audit() {
    let temp = boss_repo_with_app(
        "[settings]\nstale_exclusion_severity = \"off\"\n",
        SERVER_STATE_WITH_BUILDER,
    );
    let (results, calls) = run_mock_exclusion_audit(
        &temp,
        ExclusionStatus::Stale {
            reason: "irrelevant — the audit is disabled".to_owned(),
        },
    )
    .await;
    assert_eq!(calls, 0, "off mode must short-circuit before the check is consulted");
    assert!(
        stale_findings(&results).is_empty(),
        "audit must be disabled when set to off, got {results:?}"
    );
}

include!("tests_policy.rs");
include!("tests_external.rs");
include!("tests_fix_multipass.rs");

// ── change-scope finding filter ─────────────────────────────────────────────────

fn scoped_changeset() -> ChangeSet {
    ChangeSet::new(vec![ChangedFile {
        path: Path::new("crate/src/changed.rs").to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }])
}

fn finding_at(path: Option<&str>) -> Finding {
    Finding {
        severity: Severity::Warning,
        message: "msg".to_owned(),
        location: path.map(|p| Location {
            path: Path::new(p).to_path_buf(),
            line: Some(1),
            column: None,
        }),
        remediations: vec![],
        suggested_fix: None,
    }
}

/// A tool that over-reports relative to the change scope (e.g. clippy diagnosing
/// a whole crate when one file changed) has its out-of-scope findings dropped by
/// the framework; in-scope and location-less findings survive.
#[test]
fn scope_filter_drops_findings_outside_changeset() {
    let mut result = CheckResult {
        check_id: "clippy".to_owned(),
        findings: vec![
            finding_at(Some("crate/src/changed.rs")),
            finding_at(Some("crate/src/unchanged_sibling.rs")),
            finding_at(None),
        ],
    };
    super::scope_findings_to_changeset(&mut result, &scoped_changeset());

    assert_eq!(result.findings.len(), 2, "got: {:?}", result.findings);
    assert_eq!(
        result.findings[0].location.as_ref().map(|l| l.path.clone()),
        Some(Path::new("crate/src/changed.rs").to_path_buf())
    );
    assert!(result.findings[1].location.is_none(), "location-less findings survive");
}

/// In `--all` mode the changeset is every file in the repo, so the filter keeps
/// everything — the per-file membership check is the only mechanism; no mode flag.
#[test]
fn scope_filter_is_noop_when_all_files_are_in_changeset() {
    let all = ChangeSet::new(vec![
        ChangedFile {
            path: Path::new("crate/src/changed.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("crate/src/unchanged_sibling.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);
    let mut result = CheckResult {
        check_id: "clippy".to_owned(),
        findings: vec![
            finding_at(Some("crate/src/changed.rs")),
            finding_at(Some("crate/src/unchanged_sibling.rs")),
        ],
    };
    super::scope_findings_to_changeset(&mut result, &all);
    assert_eq!(result.findings.len(), 2);
}

// ── apply_suggested_fixes tests ───────────────────────────────────────────────

use crate::output::{FileEdit, SuggestedFix};

fn make_runner_for_tree(dir: &tempfile::TempDir) -> Runner {
    Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(dir.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(dir.path()).expect("tree")),
    )
}

fn make_result_with_fix(check_id: &str, file: &str, old_text: &str, new_text: &str) -> CheckResult {
    CheckResult {
        check_id: check_id.to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: "needs fix".to_owned(),
            location: Some(Location {
                path: PathBuf::from(file),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: Some(SuggestedFix {
                description: "auto-fix".to_owned(),
                edits: vec![FileEdit {
                    path: PathBuf::from(file),
                    old_text: old_text.to_owned(),
                    new_text: new_text.to_owned(),
                }],
            }),
        }],
    }
}

#[test]
fn apply_suggested_fixes_writes_edited_file_to_real_tree() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("a.txt"), b"hello world").expect("write a.txt");

    let runner = make_runner_for_tree(&dir);
    let result = make_result_with_fix("my-check", "a.txt", "hello", "goodbye");
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert_eq!(outcomes.len(), 1);
    let inv = &outcomes["my-check"];
    assert_eq!(inv.len(), 1);
    assert_eq!(inv[0].invocation_id, "suggested_fix");
    assert!(inv[0].error.is_none(), "expected no error, got: {:?}", inv[0].error);
    assert_eq!(inv[0].applied, vec![PathBuf::from("a.txt")]);
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"goodbye world",
        "edit must be applied to the real file"
    );
}

#[test]
fn apply_suggested_fixes_is_absent_when_no_suggested_fix_present() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("a.txt"), b"content").expect("write a.txt");

    let runner = make_runner_for_tree(&dir);
    // Finding without a suggested_fix.
    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: "problem".to_owned(),
            location: Some(Location {
                path: PathBuf::from("a.txt"),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: None,
        }],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());
    assert!(
        outcomes.is_empty(),
        "no suggested_fix → no entry (caller treats absent as no fix available)"
    );
}

#[test]
fn apply_suggested_fixes_ignores_edits_outside_fixable_set() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("a.txt"), b"hello").expect("write a.txt");
    fs::write(dir.path().join("b.txt"), b"world").expect("write b.txt");

    let runner = make_runner_for_tree(&dir);
    // suggested_fix has edits for both a.txt and b.txt, but only a.txt is fixable.
    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: "needs fix".to_owned(),
            location: Some(Location {
                path: PathBuf::from("a.txt"),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: Some(SuggestedFix {
                description: "auto-fix".to_owned(),
                edits: vec![
                    FileEdit {
                        path: PathBuf::from("a.txt"),
                        old_text: "hello".to_owned(),
                        new_text: "goodbye".to_owned(),
                    },
                    FileEdit {
                        path: PathBuf::from("b.txt"),
                        old_text: "world".to_owned(),
                        new_text: "WORLD".to_owned(),
                    },
                ],
            }),
        }],
    };
    // Only a.txt is in the fix plan.
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert_eq!(outcomes["my-check"][0].applied, vec![PathBuf::from("a.txt")]);
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"goodbye",
        "in-plan file must be fixed"
    );
    assert_eq!(
        fs::read(dir.path().join("b.txt")).unwrap(),
        b"world",
        "out-of-plan file must be untouched"
    );
}

#[test]
fn apply_suggested_fixes_skips_info_severity_findings() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("a.txt"), b"hello").expect("write a.txt");

    let runner = make_runner_for_tree(&dir);
    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![Finding {
            severity: Severity::Info, // Info → not fixed
            message: "advisory".to_owned(),
            location: Some(Location {
                path: PathBuf::from("a.txt"),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: Some(SuggestedFix {
                description: "auto-fix".to_owned(),
                edits: vec![FileEdit {
                    path: PathBuf::from("a.txt"),
                    old_text: "hello".to_owned(),
                    new_text: "goodbye".to_owned(),
                }],
            }),
        }],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());
    assert!(outcomes.is_empty(), "Info-severity findings must not be fixed");
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"hello",
        "file must be untouched"
    );
}

#[test]
fn apply_suggested_fixes_is_idempotent_when_old_text_absent() {
    // If old_text is no longer in the file (already fixed), the content is
    // unchanged and no copy-back occurs.
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("a.txt"), b"goodbye world").expect("write a.txt");

    let runner = make_runner_for_tree(&dir);
    let result = make_result_with_fix("my-check", "a.txt", "hello", "goodbye");
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert_eq!(outcomes.len(), 1);
    let inv = &outcomes["my-check"][0];
    assert!(inv.error.is_none());
    assert!(inv.applied.is_empty(), "no copy-back when content unchanged");
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"goodbye world",
        "file content unchanged"
    );
}

fn starlark_package_archive() -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    append_archive_file(
        &mut builder,
        "package.toml",
        r#"
[package]
name = "local/archive-checks"
version = "0.1.0"
"#,
    );
    append_archive_file(
        &mut builder,
        "lib/messages.checkleft",
        r#"
def debug_message() -> str:
    return "debug text added from archive"
"#,
    );
    append_archive_file(
        &mut builder,
        "text/no_debug/check.checkleft",
        r#"
load("//lib/messages", "debug_message")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if "debug" in line.text:
                findings.append(fail(
                    message = debug_message(),
                    path = file.path,
                    line = line.number,
                    column = 1,
                ))
    return findings
"#,
    );
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn starlark_version_set_archive(package_sha256: &str) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    append_archive_file(
        &mut builder,
        "package.toml",
        &format!(
            r#"
[package]
name = "local/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.archive_checks]
source = "path://packages/checks.tar.gz"
version = "0.1.0"
sha256 = "{package_sha256}"
"#
        ),
    );
    let encoder = builder.into_inner().expect("finish version set tar");
    encoder.finish().expect("finish version set gzip")
}

fn append_archive_file(builder: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, contents: &str) {
    let bytes = contents.as_bytes();
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new(bytes))
        .expect("append archive entry");
}

fn sha256_hex_for_test(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}
