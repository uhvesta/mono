use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
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
