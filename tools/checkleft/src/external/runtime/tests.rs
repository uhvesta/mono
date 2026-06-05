use std::fs;
use tempfile::tempdir;

use crate::external::{
    EXTERNAL_CHECK_API_V1, ExternalCheckArtifactPackage, ExternalCheckCapabilities,
    ExternalCheckPackage, ExternalCheckPackageImplementation,
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
