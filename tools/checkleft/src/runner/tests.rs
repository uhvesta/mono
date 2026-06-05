use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tempfile::tempdir;

use crate::check::{Check, CheckRegistry, ConfiguredCheck};
use crate::checks::register_builtin_checks;
use crate::config::ConfigResolver;
use crate::external::{
    ExternalCheckArtifactPackage, ExternalCheckExecutor, ExternalCheckImplementationRef,
    ExternalCheckPackage, ExternalCheckPackageImplementation, ExternalCheckPackageProvider,
    parse_external_check_package_manifest,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::source_tree::LocalSourceTree;

use super::Runner;

struct StaticExternalProvider {
    package: Option<ExternalCheckPackage>,
}

impl ExternalCheckPackageProvider for StaticExternalProvider {
    fn resolve(
        &self,
        _implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
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
        *self.seen_bypass_reason.lock().expect("lock bypass reason") =
            changeset.bypass_reason(&self.directive_name);
        *self.seen_change_id.lock().expect("lock change id") = changeset.change_id.clone();
        *self.seen_repository.lock().expect("lock repository") = changeset.repository.clone();

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: Vec::new(),
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
    assert_eq!(
        *seen_change_id.lock().expect("lock change id"),
        Some("235".to_owned())
    );
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
        results[0].findings[0]
            .location
            .as_ref()
            .map(|location| &location.path),
        Some(&Path::new("CHECKS.yaml").to_path_buf())
    );
    assert!(
        results[0].findings[0]
            .message
            .contains("failed to parse checks config")
    );
}

#[tokio::test]
async fn runner_reports_invalid_builtin_config_on_checks_file() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = "many"
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
    assert_eq!(results[0].check_id, "file-size");
    assert_eq!(
        results[0].findings[0]
            .location
            .as_ref()
            .map(|location| &location.path),
        Some(&Path::new("CHECKS.toml").to_path_buf())
    );
    assert!(
        results[0].findings[0]
            .message
            .contains("invalid file-size check config")
    );
    assert!(
        !results[0].findings[0]
            .message
            .contains("check execution failed")
    );
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
    assert!(
        results[0].findings[0]
            .message
            .contains("unknown implementation")
    );
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
id = "rust-giant-structs-use-builder"

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

async fn run_builder_audit(temp: &tempfile::TempDir) -> Vec<CheckResult> {
    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");
    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
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

fn stale_findings(results: &[CheckResult]) -> Vec<&Finding> {
    results
        .iter()
        .flat_map(|result| result.findings.iter())
        .filter(|finding| finding.message.contains("is no longer needed"))
        .collect()
}

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

#[tokio::test]
async fn load_bearing_exclusion_is_not_flagged() {
    let temp = boss_repo_with_app("", SERVER_STATE_WITHOUT_BUILDER);
    let results = run_builder_audit(&temp).await;
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
    let results = run_builder_audit(&temp).await;
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
    let results = run_builder_audit(&temp).await;
    assert!(
        stale_findings(&results).is_empty(),
        "audit must be disabled when set to off, got {results:?}"
    );
}

include!("tests_policy.rs");
include!("tests_external.rs");
