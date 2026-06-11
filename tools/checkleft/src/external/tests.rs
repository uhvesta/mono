use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;

use super::{
    CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider, ExternalCheckImplementationRef,
    ExternalCheckPackage, ExternalCheckPackageImplementation, ExternalCheckPackageProvider,
    FileExternalCheckPackageProvider, GeneratedExternalCheckPackageProvider, parse_external_check_package_manifest,
};

struct StaticProvider {
    package: Option<ExternalCheckPackage>,
}

impl ExternalCheckPackageProvider for StaticProvider {
    fn resolve(
        &self,
        _implementation_ref: &ExternalCheckImplementationRef,
    ) -> anyhow::Result<Option<ExternalCheckPackage>> {
        Ok(self.package.clone())
    }
}

#[test]
fn parses_component_mode_manifest() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/workflow_shell_strict/check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[provenance]
generator = "bazel"
target = "//checks/workflow_shell_strict:check_wasm"
"#;

    let package = parse_external_check_package_manifest(manifest).expect("valid manifest");
    assert!(matches!(
        package.implementation,
        ExternalCheckPackageImplementation::Component(_)
    ));
}

#[test]
fn component_mode_parses_optional_limits() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[limits]
timeout_ms = 5000
max_memory_mb = 64
"#;

    let package = parse_external_check_package_manifest(manifest).expect("valid manifest");
    let ExternalCheckPackageImplementation::Component(component) = package.implementation else {
        panic!("expected component implementation");
    };
    let limits = component.limits.expect("limits should be present");
    assert_eq!(limits.timeout_ms, Some(5000));
    assert_eq!(limits.max_memory_mb, Some(64));
}

#[test]
fn component_mode_parses_checks_allowlist() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
checks = ["workflow-shell-strict", "workflow-shell-lint"]
"#;

    let package = parse_external_check_package_manifest(manifest).expect("valid manifest");
    let ExternalCheckPackageImplementation::Component(component) = package.implementation else {
        panic!("expected component implementation");
    };
    let checks = component.checks.expect("checks allowlist should be present");
    assert_eq!(checks, vec!["workflow-shell-strict", "workflow-shell-lint"]);
}

#[test]
fn component_mode_requires_artifact_path() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("artifact_path"));
}

#[test]
fn component_mode_requires_artifact_sha256() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "check.wasm"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("artifact_sha256"));
}

#[test]
fn rejects_wasm_mode() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;

    // `wasm`/`sandbox-v1` has been removed; only `component` and `declarative` remain.
    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("unknown variant `wasm`") || message.contains("mode"),
        "unexpected error: {message}"
    );
}

#[test]
fn rejects_unknown_mode() {
    let manifest = r#"
id = "frontend-no-legacy-api"
mode = "exec"
runtime = "exec-v1"
api_version = "v1"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("unknown variant `exec`") || message.contains("mode"),
        "unexpected error: {message}"
    );
}

#[test]
fn rejects_invalid_runtime_for_component_mode() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v2"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("unsupported runtime"));
}

#[test]
fn rejects_unknown_manifest_fields() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
api_vesion = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    let message = format!("{error:#}");
    assert!(message.contains("unknown field"));
    assert!(message.contains("api_vesion"));
}

#[test]
fn rejects_non_canonical_artifact_sha256() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/workflow_shell_strict/check.wasm"
artifact_sha256 = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("canonical sha256 digest"));
}

#[test]
fn parses_generated_implementation_ref() {
    let implementation_ref =
        ExternalCheckImplementationRef::parse("generated:domain-typo-check").expect("valid generated ref");
    assert!(matches!(
        implementation_ref,
        ExternalCheckImplementationRef::Generated(ref id) if id == "domain-typo-check"
    ));
}

#[test]
fn parses_file_implementation_ref() {
    let implementation_ref =
        ExternalCheckImplementationRef::parse("checks/workflow-shell-strict/check.toml").expect("valid file ref");
    assert_eq!(
        implementation_ref,
        ExternalCheckImplementationRef::File(PathBuf::from("checks/workflow-shell-strict/check.toml"))
    );
}

#[test]
fn rejects_empty_generated_id() {
    let error = ExternalCheckImplementationRef::parse("generated:").expect_err("must fail");
    assert!(error.to_string().contains("include an id"));
}

#[test]
fn parses_bundled_implementation_ref() {
    let implementation_ref = ExternalCheckImplementationRef::parse("bundled:buildifier").expect("valid bundled ref");
    assert!(matches!(
        implementation_ref,
        ExternalCheckImplementationRef::Bundled(ref name) if name == "buildifier"
    ));
    // Display round-trips back to the canonical `bundled:` form.
    assert_eq!(implementation_ref.to_string(), "bundled:buildifier");
}

#[test]
fn rejects_empty_bundled_name() {
    let error = ExternalCheckImplementationRef::parse("bundled:").expect_err("must fail");
    assert!(error.to_string().contains("include a name"));
}

#[test]
fn accepts_namespaced_bundled_name() {
    let result = ExternalCheckImplementationRef::parse("bundled:namespace/name").expect("must succeed");
    assert_eq!(
        result,
        ExternalCheckImplementationRef::Bundled("namespace/name".to_owned())
    );
}

#[test]
fn rejects_bundled_name_with_deep_path() {
    let error = ExternalCheckImplementationRef::parse("bundled:foo/bar/baz").expect_err("must fail");
    assert!(error.to_string().contains("deeper path"), "unexpected error: {error}");
}

#[test]
fn rejects_absolute_file_implementation_ref() {
    let error = ExternalCheckImplementationRef::parse("/tmp/check.toml").expect_err("must fail");
    assert!(error.to_string().contains("absolute paths are not allowed"));
}

#[test]
fn file_provider_resolves_manifest_path() {
    let temp = tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("checks/workflow")).expect("create dirs");
    fs::write(
        temp.path().join("checks/workflow/check.toml"),
        r#"
id = "workflow-shell-strict-v2"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/workflow/check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write manifest");

    let provider = FileExternalCheckPackageProvider::new(temp.path()).expect("provider");
    let package = provider
        .resolve(&ExternalCheckImplementationRef::parse("checks/workflow/check.toml").expect("implementation"))
        .expect("resolve")
        .expect("package");

    assert_eq!(package.id, "workflow-shell-strict-v2");
}

#[test]
fn generated_provider_resolves_from_index() {
    let temp = tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("generated")).expect("create dirs");
    fs::write(
        temp.path().join("generated/domain_typo.check.toml"),
        r#"
id = "domain-typo-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/domain_typo.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write manifest");
    fs::write(
        temp.path().join("generated/index.toml"),
        r#"
version = 1

[[packages]]
implementation = "generated:domain-typo-check"
manifest = "./domain_typo.check.toml"
"#,
    )
    .expect("write index");

    let provider =
        GeneratedExternalCheckPackageProvider::from_index_path(temp.path(), &PathBuf::from("generated/index.toml"))
            .expect("provider");
    let package = provider
        .resolve(&ExternalCheckImplementationRef::parse("generated:domain-typo-check").expect("implementation"))
        .expect("resolve")
        .expect("package");

    assert_eq!(package.id, "domain-typo-check");
}

#[test]
fn generated_provider_rejects_unsupported_index_version() {
    let temp = tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("generated")).expect("create dirs");
    fs::write(
        temp.path().join("generated/index.toml"),
        r#"
version = 2
"#,
    )
    .expect("write index");

    let error =
        GeneratedExternalCheckPackageProvider::from_index_path(temp.path(), &PathBuf::from("generated/index.toml"))
            .expect_err("must reject unsupported version");
    assert!(
        error
            .to_string()
            .contains("unsupported generated external package index version")
    );
}

#[test]
fn composite_provider_reports_conflicts() {
    let package = ExternalCheckPackage {
        id: "domain-typo-check".to_owned(),
        runtime: "component-v1".to_owned(),
        api_version: "v1".to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(super::ExternalCheckComponentPackage {
            artifact_path: "check.wasm".to_owned(),
            artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            artifact_bytes: None,
            check_name: "domain-typo-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let provider = CompositeExternalCheckPackageProvider::new(vec![
        ConfiguredExternalCheckPackageProvider::new(
            "p1",
            Arc::new(StaticProvider {
                package: Some(package.clone()),
            }),
        ),
        ConfiguredExternalCheckPackageProvider::new("p2", Arc::new(StaticProvider { package: Some(package) })),
    ]);

    let error = provider
        .resolve(&ExternalCheckImplementationRef::parse("generated:domain-typo-check").expect("implementation"))
        .expect_err("must fail");
    assert!(error.to_string().contains("multiple providers"));
}

#[test]
fn composite_provider_resolves_component_package() {
    let package = ExternalCheckPackage {
        id: "my-check".to_owned(),
        runtime: super::EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: super::EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(super::ExternalCheckComponentPackage {
            artifact_path: "checks/my_check.wasm".to_owned(),
            artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            artifact_bytes: None,
            check_name: "my-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let provider = CompositeExternalCheckPackageProvider::new(vec![
        ConfiguredExternalCheckPackageProvider::new("bundled", Arc::new(StaticProvider { package: None })),
        ConfiguredExternalCheckPackageProvider::new(
            "file",
            Arc::new(StaticProvider {
                package: Some(package.clone()),
            }),
        ),
    ]);

    let resolved = provider
        .resolve(&ExternalCheckImplementationRef::parse("generated:my-check").expect("implementation"))
        .expect("resolve")
        .expect("package");

    assert_eq!(resolved.id, "my-check");
    assert!(matches!(
        resolved.implementation,
        ExternalCheckPackageImplementation::Component(_)
    ));
}

#[test]
fn composite_provider_no_conflict_when_only_one_resolves_component() {
    let package = ExternalCheckPackage {
        id: "unique-check".to_owned(),
        runtime: super::EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: super::EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(super::ExternalCheckComponentPackage {
            artifact_path: "checks/unique.wasm".to_owned(),
            artifact_sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_owned(),
            artifact_bytes: None,
            check_name: "unique-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let provider = CompositeExternalCheckPackageProvider::new(vec![
        ConfiguredExternalCheckPackageProvider::new("p-hit", Arc::new(StaticProvider { package: Some(package) })),
        ConfiguredExternalCheckPackageProvider::new("p-miss", Arc::new(StaticProvider { package: None })),
    ]);

    let resolved = provider
        .resolve(&ExternalCheckImplementationRef::parse("generated:unique-check").expect("implementation"))
        .expect("resolve")
        .expect("package");
    assert_eq!(resolved.id, "unique-check");
}

#[test]
fn component_package_check_name_matches_id_for_manifest_parsed() {
    // Verify that validate_component_implementation sets check_name == id.
    let manifest = r#"
id = "workflow-shell-strict"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/workflow_shell_strict.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;
    let package = parse_external_check_package_manifest(manifest).expect("valid manifest");
    let ExternalCheckPackageImplementation::Component(comp) = package.implementation else {
        panic!("expected Component implementation");
    };
    assert_eq!(comp.check_name, "workflow-shell-strict");
    assert_eq!(comp.check_name, package.id);
    assert!(comp.artifact_bytes.is_none());
}

#[test]
fn file_provider_resolves_component_toml_manifest() {
    let temp = tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("checks/my-component-check")).expect("create dirs");
    fs::write(
        temp.path().join("checks/my-component-check/check.toml"),
        r#"
id = "my-component-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/my_component_check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write manifest");

    let provider = FileExternalCheckPackageProvider::new(temp.path()).expect("provider");
    let package = provider
        .resolve(
            &ExternalCheckImplementationRef::parse("checks/my-component-check/check.toml").expect("implementation"),
        )
        .expect("resolve")
        .expect("package");

    assert_eq!(package.id, "my-component-check");
    let ExternalCheckPackageImplementation::Component(comp) = package.implementation else {
        panic!("expected Component implementation");
    };
    assert_eq!(comp.check_name, "my-component-check");
}
