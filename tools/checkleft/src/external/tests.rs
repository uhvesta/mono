use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;

use super::{
    CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider,
    ExternalCheckImplementationRef, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalCheckPackageProvider, FileExternalCheckPackageProvider,
    GeneratedExternalCheckPackageProvider, parse_external_check_package_manifest,
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
fn parses_wasm_mode_manifest() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v1"
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
        ExternalCheckPackageImplementation::Artifact(_)
    ));
}

#[test]
fn wasm_mode_requires_required_fields() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v1"
api_version = "v1"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("artifact_path"));
}

#[test]
fn rejects_unknown_mode() {
    let manifest = r#"
id = "frontend-no-legacy-api"
mode = "exec"
runtime = "exec-v1"
api_version = "v1"
executable_path = "bazel-bin/checks/frontend_no_legacy_api/frontend_no_legacy_api"
"#;

    // `exec` was folded into the declarative runtime; only `wasm` and
    // `declarative` modes remain.
    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("unknown variant `exec`") || message.contains("mode"),
        "unexpected error: {message}"
    );
}

#[test]
fn rejects_invalid_runtime() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v2"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("unsupported runtime"));
}

#[test]
fn rejects_duplicate_commands() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[capabilities]
commands = ["grep", "grep"]
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("duplicate command"));
}

#[test]
fn declarative_mode_rejects_capabilities() {
    let manifest = r#"
id = "buildifier-declarative"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.bzl"]

[needs.buildifier.default]
path = "buildifier"

[[invocations]]
id = "run"
run = "buildifier"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "findings", default = "error" }

[invocations.transform]
kind = "passthrough"

[capabilities]
commands = ["grep"]
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("capabilities"));
}

#[test]
fn rejects_unknown_manifest_fields() {
    let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "wasm"
runtime = "sandbox-v1"
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
mode = "wasm"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/workflow_shell_strict/check.wasm"
artifact_sha256 = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
"#;

    let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
    assert!(error.to_string().contains("canonical sha256 digest"));
}

#[test]
fn parses_generated_implementation_ref() {
    let implementation_ref = ExternalCheckImplementationRef::parse("generated:domain-typo-check")
        .expect("valid generated ref");
    assert!(matches!(
        implementation_ref,
        ExternalCheckImplementationRef::Generated(ref id) if id == "domain-typo-check"
    ));
}

#[test]
fn parses_file_implementation_ref() {
    let implementation_ref =
        ExternalCheckImplementationRef::parse("checks/workflow-shell-strict/check.toml")
            .expect("valid file ref");
    assert_eq!(
        implementation_ref,
        ExternalCheckImplementationRef::File(PathBuf::from(
            "checks/workflow-shell-strict/check.toml"
        ))
    );
}

#[test]
fn rejects_empty_generated_id() {
    let error = ExternalCheckImplementationRef::parse("generated:").expect_err("must fail");
    assert!(error.to_string().contains("include an id"));
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
mode = "wasm"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "checks/workflow/check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write manifest");

    let provider = FileExternalCheckPackageProvider::new(temp.path()).expect("provider");
    let package = provider
        .resolve(
            &ExternalCheckImplementationRef::parse("checks/workflow/check.toml")
                .expect("implementation"),
        )
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
mode = "wasm"
runtime = "sandbox-v1"
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

    let provider = GeneratedExternalCheckPackageProvider::from_index_path(
        temp.path(),
        &PathBuf::from("generated/index.toml"),
    )
    .expect("provider");
    let package = provider
        .resolve(
            &ExternalCheckImplementationRef::parse("generated:domain-typo-check")
                .expect("implementation"),
        )
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

    let error = GeneratedExternalCheckPackageProvider::from_index_path(
        temp.path(),
        &PathBuf::from("generated/index.toml"),
    )
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
        runtime: "sandbox-v1".to_owned(),
        api_version: "v1".to_owned(),
        capabilities: Default::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            super::ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_owned(),
                provenance: None,
            },
        ),
    };

    let provider = CompositeExternalCheckPackageProvider::new(vec![
        ConfiguredExternalCheckPackageProvider::new(
            "p1",
            Arc::new(StaticProvider {
                package: Some(package.clone()),
            }),
        ),
        ConfiguredExternalCheckPackageProvider::new(
            "p2",
            Arc::new(StaticProvider {
                package: Some(package),
            }),
        ),
    ]);

    let error = provider
        .resolve(
            &ExternalCheckImplementationRef::parse("generated:domain-typo-check")
                .expect("implementation"),
        )
        .expect_err("must fail");
    assert!(error.to_string().contains("multiple providers"));
}
