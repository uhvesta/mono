use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::Result;
use tempfile::tempdir;

use crate::external::{
    EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_EXEC_RUNTIME_V1, ExternalCheckArtifactPackage,
    ExternalCheckCapabilities, ExternalCheckExecPackage, ExternalCheckPackage,
    ExternalCheckPackageImplementation, ExternalSourcePackageBuilder,
};
use crate::input::ChangeSet;
use crate::output::Severity;
use crate::source_tree::LocalSourceTree;

use super::{DefaultExternalCheckExecutor, ExternalCheckExecutor, sha256_hex};

#[test]
fn executes_artifact_module_and_parses_findings() {
    let temp = tempdir().expect("temp dir");
    let output_json = r#"{"findings":[{"severity":"info","message":"hello","location":null,"remediation":null,"suggested_fix":null}]}"#;
    let output_offset = 1024_u64;
    let output_len = output_json.len() as u64;
    let encoded = (output_offset << 32) | output_len;
    let wat = format!(
        r#"(module
  (memory (export "memory") 1)
  (data (i32.const {offset}) {output:?})
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const {encoded}
  )
)"#,
        offset = output_offset,
        output = output_json,
        encoded = encoded,
    );
    let wasm_bytes = wat::parse_str(&wat).expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read wasm"));

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256,
                provenance: None,
            },
        ),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");

    assert_eq!(result.check_id, "example-check");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Info);
    assert_eq!(result.findings[0].message, "hello");
}

struct StaticSourcePackageBuilder {
    artifact: ExternalCheckArtifactPackage,
}

impl ExternalSourcePackageBuilder for StaticSourcePackageBuilder {
    fn build_source_package(
        &self,
        _package: &ExternalCheckPackage,
        _source: &crate::external::ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        Ok(self.artifact.clone())
    }
}

#[test]
fn source_mode_executes_with_built_artifact() {
    let temp = tempdir().expect("temp dir");
    let artifact_cache = tempdir().expect("artifact cache");
    let output_json = r#"{"findings":[{"severity":"warning","message":"from-source","location":null,"remediation":null,"suggested_fix":null}]}"#;
    let output_offset = 2048_u64;
    let output_len = output_json.len() as u64;
    let encoded = (output_offset << 32) | output_len;
    let wat = format!(
        r#"(module
  (memory (export "memory") 1)
  (data (i32.const {offset}) {output:?})
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const {encoded}
  )
)"#,
        offset = output_offset,
        output = output_json,
        encoded = encoded,
    );
    let wasm_bytes = wat::parse_str(&wat).expect("parse wat");
    fs::write(artifact_cache.path().join("built.wasm"), wasm_bytes).expect("write built artifact");
    let artifact_sha256 =
        sha256_hex(&fs::read(artifact_cache.path().join("built.wasm")).expect("read"));
    let source_builder = Arc::new(StaticSourcePackageBuilder {
        artifact: ExternalCheckArtifactPackage {
            artifact_path: artifact_cache
                .path()
                .join("built.wasm")
                .to_string_lossy()
                .into_owned(),
            artifact_sha256,
            provenance: None,
        },
    });
    let executor =
        DefaultExternalCheckExecutor::with_source_package_builder(temp.path(), source_builder)
            .expect("create executor");
    let package = ExternalCheckPackage {
        id: "source-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Source(
            crate::external::ExternalCheckSourcePackage {
                language: "javascript".to_owned(),
                entry: "./check.js".to_owned(),
                build_adapter: "javascript-component".to_owned(),
                sources: vec!["./check.js".to_owned()],
            },
        ),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Warning);
    assert_eq!(result.findings[0].message, "from-source");
}

#[test]
fn artifact_digest_mismatch_is_rejected() {
    let temp = tempdir().expect("temp dir");
    let wasm_bytes = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const 0
  )
)"#,
    )
    .expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .to_owned(),
                provenance: None,
            },
        ),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("must reject digest mismatch");
    assert!(error.to_string().contains("artifact sha256 mismatch"));
}
#[test]
fn core_runtime_trap_does_not_fall_back_to_component_mode() {
    let temp = tempdir().expect("temp dir");
    let wasm_bytes = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "checkleft_run") (param i32 i32) (result i64)
    unreachable
  )
)"#,
    )
    .expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read wasm"));

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256,
                provenance: None,
            },
        ),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("core trap must surface as runtime execution failure");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("external wasm check execution failed"));
    assert!(!rendered.contains("failed to compile component"));
}

#[test]
fn package_declaring_shell_command_is_rejected() {
    let temp = tempdir().expect("temp dir");
    let wasm_bytes = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const 0
  )
)"#,
    )
    .expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read"));

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities {
            commands: vec!["sh".to_owned()],
        },
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256,
                provenance: None,
            },
        ),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("shell declarations must be rejected");
    assert!(
        error
            .to_string()
            .contains("invalid command capability declaration")
    );
}

#[test]
#[cfg(unix)]
fn executes_repo_local_exec_runtime_and_parses_findings() {
    let temp = tempdir().expect("temp dir");
    let script_path = temp.path().join("check.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
input="$(cat)"
case "$input" in
  *"docs/file.md"*)
    printf '%s' '{"findings":[{"severity":"warning","message":"exec-ok","location":null,"remediation":null,"suggested_fix":null}]}'
    ;;
  *)
    printf '%s' '{"findings":[]}'
    ;;
esac
"#,
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "exec-check".to_owned(),
        runtime: EXTERNAL_CHECK_EXEC_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Exec(ExternalCheckExecPackage {
            executable_path: "check.sh".to_owned(),
            args: Vec::new(),
            provenance: None,
        }),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::new(vec![crate::input::ChangedFile {
                path: "docs/file.md".into(),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            }]),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");

    assert_eq!(result.check_id, "exec-check");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Warning);
    assert_eq!(result.findings[0].message, "exec-ok");
}

#[test]
#[cfg(unix)]
fn exec_runtime_reports_non_zero_exit_and_stderr() {
    let temp = tempdir().expect("temp dir");
    let script_path = temp.path().join("check.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
echo 'bad-output' >&2
exit 17
"#,
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "exec-check".to_owned(),
        runtime: EXTERNAL_CHECK_EXEC_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Exec(ExternalCheckExecPackage {
            executable_path: "check.sh".to_owned(),
            args: Vec::new(),
            provenance: None,
        }),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("must fail");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("exited with status"));
    assert!(rendered.contains("stderr: bad-output"));
}

#[test]
#[cfg(unix)]
fn exec_runtime_sets_bazel_bindir_for_bazel_bin_launchers() {
    let temp = tempdir().expect("temp dir");
    let bazel_bin_dir = temp.path().join("bazel-bin");
    fs::create_dir_all(&bazel_bin_dir).expect("mkdir bazel-bin");
    let script_path = bazel_bin_dir.join("check.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
if [ "${BAZEL_BINDIR:-}" != "." ]; then
  echo "expected BAZEL_BINDIR=. but got '${BAZEL_BINDIR:-}'" >&2
  exit 23
fi
printf '%s' '{"findings":[{"severity":"info","message":"bazel-bindir-ok","location":null,"remediation":null,"suggested_fix":null}]}'
"#,
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let executor = DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "exec-check".to_owned(),
        runtime: EXTERNAL_CHECK_EXEC_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Exec(ExternalCheckExecPackage {
            executable_path: "bazel-bin/check.sh".to_owned(),
            args: Vec::new(),
            provenance: None,
        }),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");

    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Info);
    assert_eq!(result.findings[0].message, "bazel-bindir-ok");
}
