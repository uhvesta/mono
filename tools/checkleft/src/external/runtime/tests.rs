use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

use crate::external::{
    EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, ExternalCheckArtifactPackage,
    ExternalCheckCapabilities, ExternalCheckPackage, ExternalCheckPackageImplementation,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff};
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

// --- component-v1 error-path tests ---

#[test]
fn component_v1_non_component_bytes_give_compile_error() {
    // Passing core-wasm bytes to the component-v1 path must fail at the compile
    // step (not silently fall back to the core path).
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
    fs::write(temp.path().join("check.wasm"), &wasm_bytes).expect("write wasm");
    let artifact_sha256 = super::sha256_hex(&wasm_bytes);

    let executor = super::DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(ExternalCheckArtifactPackage {
            artifact_path: "check.wasm".to_owned(),
            artifact_sha256,
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
        .expect_err("core wasm bytes must not parse as a component");
    assert!(
        error.to_string().contains("failed to compile component"),
        "unexpected error: {error}"
    );
}

#[test]
fn component_v1_digest_mismatch_is_rejected() {
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
    fs::write(temp.path().join("check.wasm"), &wasm_bytes).expect("write wasm");

    let executor = super::DefaultExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(ExternalCheckArtifactPackage {
            artifact_path: "check.wasm".to_owned(),
            artifact_sha256: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                .to_owned(),
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
        .expect_err("digest mismatch must be rejected");
    assert!(error.to_string().contains("artifact sha256 mismatch"));
}

// --- Type lowering unit tests ---

#[test]
fn lower_changeset_maps_fields_to_wit_types() {
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: "src/foo.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: "src/bar.rs".into(),
            kind: ChangeKind::Renamed,
            old_path: Some("src/old_bar.rs".into()),
        },
    ])
    .with_commit_description(Some("feat: add bar".to_owned()))
    .with_file_diff(
        PathBuf::from("src/foo.rs"),
        FileDiff {
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 3,
                added_lines: 2,
                removed_lines: 1,
            }],
        },
    );

    let wit_cs = super::lower_changeset(&changeset);
    assert_eq!(wit_cs.changed_files.len(), 2);
    assert_eq!(wit_cs.changed_files[0].path, "src/foo.rs");
    assert!(matches!(
        wit_cs.changed_files[0].kind,
        super::wit_types::ChangeKind::Modified
    ));
    assert_eq!(wit_cs.changed_files[1].old_path.as_deref(), Some("src/old_bar.rs"));
    assert_eq!(wit_cs.file_diffs.len(), 1);
    assert_eq!(wit_cs.file_diffs[0].path, "src/foo.rs");
    assert_eq!(wit_cs.file_diffs[0].hunks.len(), 1);
    assert_eq!(wit_cs.file_diffs[0].hunks[0].old_start, 1_u32);
    assert_eq!(wit_cs.file_diffs[0].hunks[0].added_lines, 2_u32);
    assert_eq!(wit_cs.commit_description.as_deref(), Some("feat: add bar"));
}

// --- Type lifting unit tests ---

#[test]
fn lift_finding_maps_all_fields() {
    let wit_finding = super::wit_types::Finding {
        severity: super::wit_types::Severity::Error,
        message: "something is wrong".to_owned(),
        location: Some(super::wit_types::Location {
            path: "src/lib.rs".to_owned(),
            line: Some(42),
            column: Some(7),
        }),
        remediations: vec!["fix it".to_owned()],
        suggested_fix: Some(super::wit_types::SuggestedFix {
            description: "auto-fix".to_owned(),
            edits: vec![super::wit_types::FileEdit {
                path: "src/lib.rs".to_owned(),
                old_text: "bad".to_owned(),
                new_text: "good".to_owned(),
            }],
        }),
    };

    let finding = super::lift_finding(wit_finding);
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.message, "something is wrong");
    assert_eq!(finding.location.as_ref().unwrap().path, PathBuf::from("src/lib.rs"));
    assert_eq!(finding.location.as_ref().unwrap().line, Some(42));
    assert_eq!(finding.location.as_ref().unwrap().column, Some(7));
    assert_eq!(finding.remediations, vec!["fix it".to_owned()]);
    let fix = finding.suggested_fix.as_ref().unwrap();
    assert_eq!(fix.description, "auto-fix");
    assert_eq!(fix.edits[0].path, PathBuf::from("src/lib.rs"));
    assert_eq!(fix.edits[0].old_text, "bad");
    assert_eq!(fix.edits[0].new_text, "good");
}

#[test]
fn lift_finding_with_no_location_or_fix() {
    let wit_finding = super::wit_types::Finding {
        severity: super::wit_types::Severity::Warning,
        message: "minor issue".to_owned(),
        location: None,
        remediations: vec![],
        suggested_fix: None,
    };

    let finding = super::lift_finding(wit_finding);
    assert_eq!(finding.severity, Severity::Warning);
    assert!(finding.location.is_none());
    assert!(finding.suggested_fix.is_none());
    assert!(finding.remediations.is_empty());
}
