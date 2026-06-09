use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

use crate::external::{
    EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, ExternalCheckArtifactPackage,
    ExternalCheckCapabilities, ExternalCheckComponentLimits, ExternalCheckPackage,
    ExternalCheckPackageImplementation,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff};
use crate::output::Severity;
use crate::source_tree::LocalSourceTree;

use super::{
    DefaultExternalCheckExecutor, EPOCH_DEADLINE_NEVER, ExternalCheckExecutor, HostState,
    MemoryLimiter, WASM_PAGE_SIZE_BYTES, build_wasmtime_engine, is_interrupt_error,
    resolve_component_limits, sha256_hex,
};
use wasmtime::{Instance, Module, Store};

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
        error.to_string().contains("failed to precompile component"),
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

// --- T4: access-scope lifting unit tests ---

#[test]
fn lift_access_scope_none_defaults_to_modified_only() {
    let scope = super::lift_access_scope(None);
    assert!(matches!(scope, crate::external::sandbox::AccessScope::ModifiedOnly));
}

#[test]
fn lift_access_scope_modified_only_variant() {
    let scope = super::lift_access_scope(Some(&super::wit_types::AccessScope::ModifiedOnly));
    assert!(matches!(scope, crate::external::sandbox::AccessScope::ModifiedOnly));
}

#[test]
fn lift_access_scope_whole_repo_variant() {
    let scope = super::lift_access_scope(Some(&super::wit_types::AccessScope::WholeRepo));
    assert!(matches!(scope, crate::external::sandbox::AccessScope::WholeRepo));
}

#[test]
fn lift_access_scope_globs_variant_preserves_patterns() {
    let patterns = vec!["**/*.rs".to_owned(), "**/Cargo.toml".to_owned()];
    let scope =
        super::lift_access_scope(Some(&super::wit_types::AccessScope::Globs(patterns.clone())));
    match scope {
        crate::external::sandbox::AccessScope::Globs(got) => assert_eq!(got, patterns),
        other => panic!("expected Globs, got {other:?}"),
    }
}

// --- T4: WASI sandbox integration tests ---
//
// These tests verify that the executor correctly creates the FS sandbox from
// the declared access scope and that files outside the scope are absent from
// the sandbox directory (structural enforcement — no WASI component binary is
// needed to observe the preopened directory contents).

#[test]
fn sandbox_is_populated_only_with_changeset_files_for_modified_only_scope() {
    use crate::external::sandbox::{AccessScope, HostCeiling, create_sandbox};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use std::path::Path;

    struct MapTree(std::collections::HashMap<PathBuf, Vec<u8>>);
    impl SourceTree for MapTree {
        fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found: {}", path.display()))
        }
        fn exists(&self, path: &Path) -> bool {
            self.0.contains_key(path)
        }
        fn list_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn glob(&self, _: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
    }

    let tree = MapTree(
        [
            ("changed.rs", b"fn changed() {}".as_slice()),
            ("bystander.rs", b"fn bystander() {}".as_slice()),
        ]
        .into_iter()
        .map(|(p, c)| (PathBuf::from(p), c.to_vec()))
        .collect(),
    );

    let cs = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("changed.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let dir = tempdir().expect("temp dir");
    let ceiling = HostCeiling::new(dir.path());
    let sandbox = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &ceiling)
        .expect("create sandbox");

    assert!(
        sandbox.root.path().join("changed.rs").exists(),
        "changed.rs must be in sandbox"
    );
    assert!(
        !sandbox.root.path().join("bystander.rs").exists(),
        "bystander.rs must NOT be in sandbox (outside scope)"
    );
}

#[test]
fn sandbox_grant_includes_glob_matched_files() {
    use crate::external::sandbox::{AccessScope, HostCeiling, create_sandbox};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use std::collections::HashMap;
    use std::path::Path;
    use globset::{Glob, GlobSetBuilder};

    struct GlobTree(HashMap<PathBuf, Vec<u8>>);
    impl SourceTree for GlobTree {
        fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found: {}", path.display()))
        }
        fn exists(&self, path: &Path) -> bool {
            self.0.contains_key(path)
        }
        fn list_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn glob(&self, pattern: &str) -> anyhow::Result<Vec<PathBuf>> {
            let mut builder = GlobSetBuilder::new();
            builder.add(Glob::new(pattern)?);
            let set = builder.build()?;
            let mut hits: Vec<PathBuf> = self
                .0
                .keys()
                .filter(|p| set.is_match(p.as_path()))
                .cloned()
                .collect();
            hits.sort();
            Ok(hits)
        }
    }

    let tree = GlobTree(
        [
            ("Cargo.toml", b"[package]".as_slice()),
            ("lib/Cargo.toml", b"[package]".as_slice()),
            ("src/main.rs", b"fn main() {}".as_slice()),
        ]
        .into_iter()
        .map(|(p, c)| (PathBuf::from(p), c.to_vec()))
        .collect(),
    );

    let cs = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("src/main.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let dir = tempdir().expect("temp dir");
    let ceiling = HostCeiling::new(dir.path());
    let sandbox = create_sandbox(
        &cs,
        AccessScope::Globs(vec!["**/Cargo.toml".to_owned()]),
        &tree,
        &ceiling,
    )
    .expect("create sandbox");

    // Changeset file + both Cargo.toml matches
    assert!(sandbox.root.path().join("src/main.rs").exists(), "changeset file must be granted");
    assert!(sandbox.root.path().join("Cargo.toml").exists(), "root Cargo.toml must be granted");
    assert!(sandbox.root.path().join("lib/Cargo.toml").exists(), "lib Cargo.toml must be granted");
}

#[test]
fn sandbox_deny_rejects_traversal_escape_in_changeset() {
    use crate::external::sandbox::{AccessScope, HostCeiling, create_sandbox};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use std::path::Path;

    struct EmptyTree;
    impl SourceTree for EmptyTree {
        fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("not found: {}", path.display())
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn list_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn glob(&self, _: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
    }

    let cs = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("../../etc/passwd"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let dir = tempdir().expect("temp dir");
    let ceiling = HostCeiling::new(dir.path());
    let err = create_sandbox(&cs, AccessScope::ModifiedOnly, &EmptyTree, &ceiling)
        .expect_err("traversal escape must be rejected");

    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("traversal") || rendered.contains("invalid path"),
        "expected traversal error, got: {rendered}"
    );
}

#[test]
fn sandbox_deny_rejects_absolute_path_in_changeset() {
    use crate::external::sandbox::{AccessScope, HostCeiling, create_sandbox};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use std::path::Path;

    struct EmptyTree;
    impl SourceTree for EmptyTree {
        fn read_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("not found: {}", path.display())
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn list_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn glob(&self, _: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
    }

    let cs = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("/etc/passwd"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let dir = tempdir().expect("temp dir");
    let ceiling = HostCeiling::new(dir.path());
    let err = create_sandbox(&cs, AccessScope::ModifiedOnly, &EmptyTree, &ceiling)
        .expect_err("absolute path must be rejected");

    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("absolute") || rendered.contains("invalid path"),
        "expected absolute-path error, got: {rendered}"
    );
}

#[test]
fn build_component_v1_linker_succeeds() {
    use wasmtime::{Config, Engine};
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = Engine::new(&config).expect("build engine");
    super::build_component_v1_linker(&engine).expect("build component-v1 linker with WASI");
}

#[test]
fn host_state_with_empty_wasi_does_not_panic() {
    let _ = super::HostState::with_empty_wasi();
}

#[test]
fn host_state_with_sandbox_root_preopens_the_directory() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("probe.txt"), b"hello").expect("write probe file");
    let state =
        super::HostState::with_sandbox_root(dir.path()).expect("build HostState with sandbox root");
    // The HostState was created — the preopened_dir call did not fail.
    // (Runtime behavior is verified by the full component integration path once a
    // test component binary is available.)
    drop(state);
}

// --- Limit / timeout policy tests (T5) ---

#[test]
fn resolve_limits_uses_defaults_when_none() {
    let (timeout_ms, max_bytes) = resolve_component_limits(None);
    assert_eq!(timeout_ms, super::DEFAULT_COMPONENT_TIMEOUT_MS);
    assert_eq!(
        max_bytes,
        super::DEFAULT_COMPONENT_MAX_MEMORY_MB as usize * 1024 * 1024
    );
}

#[test]
fn resolve_limits_respects_manifest_overrides() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(2_000),
        max_memory_mb: Some(64),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits));
    assert_eq!(timeout_ms, 2_000);
    assert_eq!(max_bytes, 64 * 1024 * 1024);
}

#[test]
fn resolve_limits_clamps_to_host_ceiling() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(super::HOST_CEILING_TIMEOUT_MS + 60_000),
        max_memory_mb: Some(super::HOST_CEILING_MAX_MEMORY_MB + 256),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits));
    assert_eq!(timeout_ms, super::HOST_CEILING_TIMEOUT_MS);
    assert_eq!(
        max_bytes,
        super::HOST_CEILING_MAX_MEMORY_MB as usize * 1024 * 1024
    );
}

#[test]
fn resolve_limits_partial_override_timeout_only() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(1_000),
        max_memory_mb: None,
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits));
    assert_eq!(timeout_ms, 1_000);
    assert_eq!(
        max_bytes,
        super::DEFAULT_COMPONENT_MAX_MEMORY_MB as usize * 1024 * 1024
    );
}

#[test]
fn resolve_limits_partial_override_memory_only() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: None,
        max_memory_mb: Some(128),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits));
    assert_eq!(timeout_ms, super::DEFAULT_COMPONENT_TIMEOUT_MS);
    assert_eq!(max_bytes, 128 * 1024 * 1024);
}

#[test]
fn memory_limiter_allows_growth_within_cap() {
    let mut limiter = MemoryLimiter {
        max_bytes: 1024 * 1024,
    };
    assert!(
        wasmtime::ResourceLimiter::memory_growing(&mut limiter, 0, 512 * 1024, None).unwrap(),
        "growth within cap should be allowed"
    );
}

#[test]
fn memory_limiter_allows_growth_exactly_at_cap() {
    let mut limiter = MemoryLimiter {
        max_bytes: 1024 * 1024,
    };
    assert!(
        wasmtime::ResourceLimiter::memory_growing(&mut limiter, 0, 1024 * 1024, None).unwrap(),
        "growth exactly at cap should be allowed"
    );
}

#[test]
fn memory_limiter_rejects_growth_beyond_cap() {
    let mut limiter = MemoryLimiter {
        max_bytes: 512 * 1024,
    };
    assert!(
        !wasmtime::ResourceLimiter::memory_growing(&mut limiter, 0, 1024 * 1024, None).unwrap(),
        "growth beyond cap should be rejected"
    );
}

/// Verifies that the epoch-interruption mechanism fires when the deadline is
/// exceeded. Uses a tight spin loop to guarantee an epoch check point is hit.
/// This exercises the engine configuration and epoch-deadline semantics used by
/// `execute_component_v1_artifact`.
#[test]
fn epoch_deadline_interrupts_spin_loop() {
    let engine = Arc::new(build_wasmtime_engine().unwrap());

    let wasm = wat::parse_str("(module (func (export \"spin\") (loop (br 0))))").unwrap();
    let module = Module::new(&engine, &wasm).unwrap();

    let mut store: Store<()> = Store::new(&engine, ());
    // Disable fuel limit so only the epoch fires.
    store.set_fuel(u64::MAX).unwrap();
    // Deadline = 1 tick from now.
    store.set_epoch_deadline(1);

    // Advance epoch past the deadline before executing.
    engine.increment_epoch();
    engine.increment_epoch();

    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let spin: wasmtime::TypedFunc<(), ()> =
        instance.get_typed_func(&mut store, "spin").unwrap();
    let err = spin
        .call(&mut store, ())
        .map_err(anyhow::Error::from)
        .expect_err("execution should be interrupted by epoch deadline");

    assert!(
        is_interrupt_error(&err),
        "expected epoch Trap::Interrupt, got: {err:#}"
    );
}

/// Verifies that the `ResourceLimiter` causes `memory.grow` to return -1 when
/// the requested size would exceed the cap installed on the store. Uses
/// `HostState` and `MemoryLimiter` directly to mirror what
/// `execute_component_v1_artifact` sets up.
#[test]
fn memory_cap_trip_via_resource_limiter() {
    let engine = build_wasmtime_engine().unwrap();

    // Allow exactly 1 wasm page (64 KiB).
    let one_page = WASM_PAGE_SIZE_BYTES;
    let mut store: Store<HostState> = Store::new(&engine, HostState::new(one_page));
    store.limiter(|state| &mut state.limiter);
    store.set_fuel(u64::MAX).unwrap();
    // This test exercises the ResourceLimiter, not epoch timeout. Disable epoch
    // so the default epoch-0 deadline does not trap immediately.
    store.set_epoch_deadline(EPOCH_DEADLINE_NEVER);

    // Module starts with 1 page and tries to grow by 1 more.
    let wasm = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "try_grow") (result i32)
    i32.const 1
    memory.grow
  )
)"#,
    )
    .unwrap();
    let module = Module::new(&engine, &wasm).unwrap();
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let try_grow: wasmtime::TypedFunc<(), i32> =
        instance.get_typed_func(&mut store, "try_grow").unwrap();

    // memory.grow returns -1 (as i32) when the ResourceLimiter rejects growth.
    let result = try_grow
        .call(&mut store, ())
        .expect("call itself should succeed");
    assert_eq!(
        result, -1,
        "memory.grow must return -1 when the cap is exceeded"
    );
}
