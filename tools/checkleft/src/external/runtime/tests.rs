use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

use crate::external::{
    EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, ExternalCheckComponentLimits,
    ExternalCheckComponentPackage, ExternalCheckPackage, ExternalCheckPackageImplementation,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff, SourceTree, TreeVersion};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::source_tree::LocalSourceTree;

use super::{
    BASE_COMPONENT_TIMEOUT_MS, EPOCH_DEADLINE_NEVER, ExternalCheckExecutor, HOST_CEILING_TIMEOUT_MS, HostState,
    MemoryLimiter, PER_FILE_COMPONENT_TIMEOUT_MS, apply_struct_exclusions, build_wasmtime_engine, is_interrupt_error,
    lower_changeset, lower_check_input, resolve_component_limits,
};
use wasmtime::{Instance, Module, Store};

struct NoopSourceTree;

impl SourceTree for NoopSourceTree {
    fn read_file(&self, _path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("NoopSourceTree: no files available")
    }

    fn read_file_versioned(&self, _path: &std::path::Path, _version: TreeVersion) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("NoopSourceTree: no files available")
    }

    fn exists(&self, _path: &std::path::Path) -> bool {
        false
    }

    fn list_dir(&self, _path: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
        Ok(vec![])
    }

    fn glob(&self, _pattern: &str) -> anyhow::Result<Vec<std::path::PathBuf>> {
        Ok(vec![])
    }
}

// --- component-v1 error-path tests ---

#[test]
fn component_v1_non_component_bytes_give_compile_error() {
    // Passing core-wasm bytes to the component-v1 path must fail at the compile
    // step (not silently succeed via some other path).
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

    let executor = super::DefaultExternalCheckExecutor::new_with_cache(temp.path(), temp.path().join("cache"))
        .expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
            artifact_path: "check.wasm".to_owned(),
            artifact_sha256,
            artifact_bytes: None,
            check_name: "example-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
            std::path::Path::new(""),
            None,
        )
        .expect_err("core wasm bytes must not parse as a component");
    let msg = error.to_string();
    assert!(
        msg.contains("failed to precompile component") || msg.contains("failed to compile component"),
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

    let executor = super::DefaultExternalCheckExecutor::new_with_cache(temp.path(), temp.path().join("cache"))
        .expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
            artifact_path: "check.wasm".to_owned(),
            artifact_sha256: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned(),
            artifact_bytes: None,
            check_name: "example-check".to_owned(),
            limits: None,
            checks: None,
            provenance: None,
        }),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
            std::path::Path::new(""),
            None,
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

    let wit_cs = lower_changeset(&changeset, &NoopSourceTree);
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
    let scope = super::lift_access_scope(Some(&super::wit_types::AccessScope::Globs(patterns.clone())));
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
    let sandbox = create_sandbox(&cs, AccessScope::ModifiedOnly, &tree, &ceiling).expect("create sandbox");

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
    use globset::{Glob, GlobSetBuilder};
    use std::collections::HashMap;
    use std::path::Path;

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
            let mut hits: Vec<PathBuf> = self.0.keys().filter(|p| set.is_match(p.as_path())).cloned().collect();
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
    assert!(
        sandbox.root.path().join("src/main.rs").exists(),
        "changeset file must be granted"
    );
    assert!(
        sandbox.root.path().join("Cargo.toml").exists(),
        "root Cargo.toml must be granted"
    );
    assert!(
        sandbox.root.path().join("lib/Cargo.toml").exists(),
        "lib Cargo.toml must be granted"
    );
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
    let state = super::HostState::with_sandbox_root(dir.path(), usize::MAX).expect("build HostState with sandbox root");
    // The HostState was created — the preopened_dir call did not fail.
    // (Runtime behavior is verified by the full component integration path once a
    // test component binary is available.)
    drop(state);
}

// --- Limit / timeout policy tests (T5) ---

#[test]
fn resolve_limits_uses_proportional_default_when_none() {
    // No limits, 0 files → BASE only
    let (timeout_ms, max_bytes) = resolve_component_limits(None, 0);
    assert_eq!(timeout_ms, BASE_COMPONENT_TIMEOUT_MS);
    assert_eq!(max_bytes, super::DEFAULT_COMPONENT_MAX_MEMORY_MB as usize * 1024 * 1024);
}

#[test]
fn resolve_limits_proportional_scales_with_file_count() {
    let (t5, _) = resolve_component_limits(None, 5);
    assert_eq!(t5, BASE_COMPONENT_TIMEOUT_MS + PER_FILE_COMPONENT_TIMEOUT_MS * 5);

    let (t50, _) = resolve_component_limits(None, 50);
    assert_eq!(t50, BASE_COMPONENT_TIMEOUT_MS + PER_FILE_COMPONENT_TIMEOUT_MS * 50);

    // Large N: verify proportional is strictly larger than small N.
    let (t500, _) = resolve_component_limits(None, 500);
    assert!(t500 > t50, "500-file timeout must exceed 50-file timeout");
}

#[test]
fn resolve_limits_proportional_clamped_to_ceiling() {
    // A large enough file count must hit the ceiling.
    let n_huge = (HOST_CEILING_TIMEOUT_MS / PER_FILE_COMPONENT_TIMEOUT_MS + 1) as usize;
    let (timeout_ms, _) = resolve_component_limits(None, n_huge);
    assert_eq!(
        timeout_ms, HOST_CEILING_TIMEOUT_MS,
        "proportional timeout must be clamped to HOST_CEILING_TIMEOUT_MS for very large N"
    );
}

#[test]
fn resolve_limits_respects_manifest_overrides() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(2_000),
        max_memory_mb: Some(64),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits), 0);
    assert_eq!(timeout_ms, 2_000);
    assert_eq!(max_bytes, 64 * 1024 * 1024);
}

#[test]
fn resolve_limits_explicit_override_ignores_file_count() {
    // An explicit manifest timeout must be used as-is regardless of n_files.
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(10_000),
        max_memory_mb: None,
    };
    let (t_0, _) = resolve_component_limits(Some(&limits), 0);
    let (t_500, _) = resolve_component_limits(Some(&limits), 500);
    assert_eq!(t_0, 10_000, "explicit override must be applied with 0 files");
    assert_eq!(t_500, 10_000, "explicit override must not scale with file count");
}

#[test]
fn resolve_limits_clamps_to_host_ceiling() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(HOST_CEILING_TIMEOUT_MS + 60_000),
        max_memory_mb: Some(super::HOST_CEILING_MAX_MEMORY_MB + 256),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits), 0);
    assert_eq!(timeout_ms, HOST_CEILING_TIMEOUT_MS);
    assert_eq!(max_bytes, super::HOST_CEILING_MAX_MEMORY_MB as usize * 1024 * 1024);
}

#[test]
fn resolve_limits_partial_override_timeout_only() {
    let limits = ExternalCheckComponentLimits {
        timeout_ms: Some(1_000),
        max_memory_mb: None,
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits), 0);
    assert_eq!(timeout_ms, 1_000);
    assert_eq!(max_bytes, super::DEFAULT_COMPONENT_MAX_MEMORY_MB as usize * 1024 * 1024);
}

#[test]
fn resolve_limits_partial_override_memory_only() {
    // No explicit timeout → proportional default (n_files=0 means BASE only).
    let limits = ExternalCheckComponentLimits {
        timeout_ms: None,
        max_memory_mb: Some(128),
    };
    let (timeout_ms, max_bytes) = resolve_component_limits(Some(&limits), 0);
    assert_eq!(
        timeout_ms, BASE_COMPONENT_TIMEOUT_MS,
        "no explicit timeout must yield BASE with 0 files"
    );
    assert_eq!(max_bytes, 128 * 1024 * 1024);
}

#[test]
fn memory_limiter_allows_growth_within_cap() {
    let mut limiter = MemoryLimiter { max_bytes: 1024 * 1024 };
    assert!(
        wasmtime::ResourceLimiter::memory_growing(&mut limiter, 0, 512 * 1024, None).unwrap(),
        "growth within cap should be allowed"
    );
}

#[test]
fn memory_limiter_allows_growth_exactly_at_cap() {
    let mut limiter = MemoryLimiter { max_bytes: 1024 * 1024 };
    assert!(
        wasmtime::ResourceLimiter::memory_growing(&mut limiter, 0, 1024 * 1024, None).unwrap(),
        "growth exactly at cap should be allowed"
    );
}

#[test]
fn memory_limiter_rejects_growth_beyond_cap() {
    let mut limiter = MemoryLimiter { max_bytes: 512 * 1024 };
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
    // Deadline = 1 tick from now (epoch is the only safety net; fuel is disabled).
    store.set_epoch_deadline(1);

    // Advance epoch past the deadline before executing.
    engine.increment_epoch();
    engine.increment_epoch();

    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let spin: wasmtime::TypedFunc<(), ()> = instance.get_typed_func(&mut store, "spin").unwrap();
    let err = spin
        .call(&mut store, ())
        .map_err(anyhow::Error::from)
        .expect_err("execution should be interrupted by epoch deadline");

    assert!(is_interrupt_error(&err), "expected epoch Trap::Interrupt, got: {err:#}");
}

/// Verifies that the `ResourceLimiter` causes `memory.grow` to return -1 when
/// the requested size would exceed the cap installed on the store. Uses
/// `HostState` and `MemoryLimiter` directly to mirror what
/// `execute_component_v1_artifact` sets up.
#[test]
fn memory_cap_trip_via_resource_limiter() {
    let engine = build_wasmtime_engine().unwrap();

    // Allow exactly 1 wasm page (64 KiB).
    let one_page = 65_536_usize;
    let mut store: Store<HostState> = Store::new(&engine, HostState::new(one_page));
    store.limiter(|state| &mut state.limiter);
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
    let try_grow: wasmtime::TypedFunc<(), i32> = instance.get_typed_func(&mut store, "try_grow").unwrap();

    // memory.grow returns -1 (as i32) when the ResourceLimiter rejects growth.
    let result = try_grow.call(&mut store, ()).expect("call itself should succeed");
    assert_eq!(result, -1, "memory.grow must return -1 when the cap is exceeded");
}

// --- Build-time .cwasm fixture parity ---

/// Guard the build-time precompile fix: under `bazel test`, the precompiled
/// `.cwasm` fixture directory MUST contain an entry whose filename is the
/// canonical cache key for the bundled `rust/giant-structs` component. If that
/// fixture is missing (e.g. a cache-key axis drifted — wasmtime version, engine
/// config, host target, or the hashing itself), the heavy tests would silently
/// cold-miss and JIT-compile the component at runtime, re-opening the 60 s
/// `checkleft_lib_test` timeout. We want that to fail loudly here instead.
///
/// No-ops outside Bazel (`cargo test`), where no fixture is staged.
#[test]
fn precompiled_cwasm_fixture_is_keyed_for_giant_structs() {
    use crate::external::{
        BundledExternalCheckPackageProvider, ExternalCheckImplementationRef, ExternalCheckPackageProvider as _,
        cache_file_name,
    };

    let Some(dir) = crate::external::test_support::precompiled_cwasm_dir() else {
        return; // not under `bazel test`; nothing staged.
    };

    let package = BundledExternalCheckPackageProvider
        .resolve(&ExternalCheckImplementationRef::Bundled(
            "rust/giant-structs".to_owned(),
        ))
        .expect("resolve")
        .expect("bundled package must exist");
    let ExternalCheckPackageImplementation::Component(component) = package.implementation else {
        panic!("rust/giant-structs must be a component package");
    };
    let bytes = component.artifact_bytes.expect("bundled component carries its bytes");

    let expected = cache_file_name(bytes);
    let fixture = dir.join(&expected);
    assert!(
        fixture.exists(),
        "precompiled .cwasm fixture `{expected}` is missing from {} — the giant-structs tests \
         would JIT-compile at runtime (a cache-key axis drifted). Rebuild \
         //tools/checkleft:precompiled_cwasm / check CHECKLEFT_WASMTIME_VERSION + ENGINE_CONFIG_KEY.",
        dir.display(),
    );
}

// --- T10: rust-giant-structs-use-builder end-to-end test ---
//
// This is the acceptance proof for the CM-wasm project: the check is authored
// on the guest SDK, built end-to-end under bazel via the rust_wasm_component
// rule (T9), bundled via the BundledExternalCheckPackageProvider (T8), and run
// through the full component-v1 host (T3-T6) with a modified-only sandbox.

#[test]
fn bundled_giant_structs_check_finds_violation_in_rs_file() {
    use crate::external::{
        BundledExternalCheckPackageProvider, ExternalCheckImplementationRef, ExternalCheckPackageProvider as _,
    };
    use std::path::Path;

    // A Rust source file with a 6-field struct and no builder derive — must trigger.
    const VIOLATION_SOURCE: &str = r#"pub struct GiantStruct {
    a: String,
    b: String,
    c: String,
    d: String,
    e: String,
    f: String,
}
"#;

    // Create a sandbox with the .rs file as the only changed file.
    let temp = tempdir().expect("temp dir");
    fs::write(temp.path().join("src.rs"), VIOLATION_SOURCE).expect("write source");

    let tree = LocalSourceTree::new(temp.path()).expect("source tree");
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("src.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    // Resolve the bundled component package for the check.
    let provider = BundledExternalCheckPackageProvider;
    let package = provider
        .resolve(&ExternalCheckImplementationRef::Bundled(
            "rust/giant-structs".to_owned(),
        ))
        .expect("resolve")
        .expect("bundled package must exist");

    // Run through the full component-v1 host with modified-only sandbox (the default).
    let executor = crate::external::test_support::executor_with_precompiled_cache(temp.path());
    let result = executor
        .execute(
            &package,
            &changeset,
            &tree,
            &toml::Value::Table(Default::default()),
            std::path::Path::new(""),
            None,
        )
        .expect("execute");

    assert_eq!(result.check_id, "rust/giant-structs");
    assert_eq!(result.findings.len(), 1, "expected exactly one finding for GiantStruct");

    let finding = &result.findings[0];
    assert!(
        finding.message.contains("GiantStruct"),
        "finding message must mention the struct name; got: {}",
        finding.message
    );
    assert!(
        finding.message.contains("bon::Builder"),
        "finding message must mention bon::Builder; got: {}",
        finding.message
    );
    // Location must point at the .rs file the guest read from the sandbox.
    let loc = finding.location.as_ref().expect("finding must have location");
    assert_eq!(loc.path, Path::new("src.rs"));
}

// NOTE: the modified-only scope behavior (a file NOT in the changeset is never
// read even when it contains a violation) is not round-tripped through the real
// component here. Its mechanism is proven structurally — the sandbox simply
// excludes out-of-scope files — by
// `sandbox_is_populated_only_with_changeset_files_for_modified_only_scope` above,
// and the check's own detection logic is covered by the native unit tests in the
// giant-structs check crate. A full wasm execution just to re-observe "no finding"
// would be redundant matrix coverage on the slow layer.

/// Regression test: the check must complete without crashing when given a large
/// Rust file (~3100 lines). Epoch-based timeout is the safety net; this test
/// confirms large files parse cleanly without triggering it.
#[test]
fn bundled_giant_structs_check_handles_large_rs_file() {
    use crate::external::{
        BundledExternalCheckPackageProvider, ExternalCheckImplementationRef, ExternalCheckPackageProvider as _,
    };
    use std::path::Path;

    // Build a ~3100-line source: many harmless one-line functions plus one
    // violating struct at the end. The fuel limit must not be the binding
    // constraint, so the check must run to completion and return a finding.
    let source = build_large_rs_source_with_violation(3100);
    assert!(
        source.lines().count() >= 3100,
        "source must be at least 3100 lines (got {})",
        source.lines().count()
    );

    let temp = tempdir().expect("temp dir");
    fs::write(temp.path().join("large.rs"), &source).expect("write source");

    let tree = LocalSourceTree::new(temp.path()).expect("source tree");
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("large.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let provider = BundledExternalCheckPackageProvider;
    let package = provider
        .resolve(&ExternalCheckImplementationRef::Bundled(
            "rust/giant-structs".to_owned(),
        ))
        .expect("resolve")
        .expect("bundled package must exist");

    let executor = crate::external::test_support::executor_with_precompiled_cache(temp.path());
    let result = executor
        .execute(
            &package,
            &changeset,
            &tree,
            &toml::Value::Table(Default::default()),
            std::path::Path::new(""),
            None,
        )
        .expect("check must complete without fuel exhaustion or timeout on a large file");

    assert_eq!(result.check_id, "rust/giant-structs");
    assert_eq!(
        result.findings.len(),
        1,
        "expected exactly one finding for the violating struct at the end of the large file"
    );
    let loc = result.findings[0]
        .location
        .as_ref()
        .expect("finding must have location");
    assert_eq!(loc.path, Path::new("large.rs"));
}

/// Build a Rust source string with approximately `line_count` lines: a block of
/// single-line functions followed by one 8-field struct that violates the rule.
fn build_large_rs_source_with_violation(line_count: usize) -> String {
    let struct_lines = 10; // pub struct { + 8 field lines + closing brace
    let func_count = line_count.saturating_sub(struct_lines);
    let mut s = String::with_capacity(line_count * 24);
    for i in 0..func_count {
        s.push_str(&format!("fn f{i}() {{}}\n"));
    }
    s.push_str("pub struct LargeViolation {\n");
    for field in ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'] {
        s.push_str(&format!("    {field}: u32,\n"));
    }
    s.push_str("}\n");
    s
}

// --- Host-side exclusion helper unit tests ---

#[test]
fn struct_name_from_finding_extracts_first_backtick_token() {
    assert_eq!(
        super::struct_name_from_finding("struct `ServerState` has more than 5 named fields but lacks `#[derive(..)]`"),
        Some("ServerState")
    );
    // No backtick pair → None (fail-safe: the finding is never suppressed).
    assert_eq!(super::struct_name_from_finding("no backticks here"), None);
    assert_eq!(super::struct_name_from_finding("only one ` backtick"), None);
}

#[test]
fn scope_exclude_globs_prefixes_only_glob_keys() {
    let config = toml::Value::Table(toml::toml! {
        max_lines = 2
        exclude_files = ["a.rs", "nested/*.rs"]
        exclude_globs = ["b.rs"]
    });
    let scoped = super::scope_exclude_globs_to_repo(&config, std::path::Path::new("sub/dir"));
    let table = scoped.as_table().unwrap();
    let files: Vec<_> = table["exclude_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(files, vec!["sub/dir/a.rs", "sub/dir/nested/*.rs"]);
    let globs: Vec<_> = table["exclude_globs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(globs, vec!["sub/dir/b.rs"]);
    // Non-glob keys are untouched.
    assert_eq!(table["max_lines"].as_integer(), Some(2));
}

#[test]
fn scope_exclude_globs_is_noop_at_repo_root() {
    let config = toml::Value::Table(toml::toml! {
        exclude_files = ["a.rs"]
    });
    let scoped = super::scope_exclude_globs_to_repo(&config, std::path::Path::new(""));
    let files: Vec<_> = scoped.as_table().unwrap()["exclude_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(files, vec!["a.rs"], "repo-root config must be left untouched");
}

// --- Host-side exclusion resolution end-to-end tests ---
//
// These exercise the full executor → component path and prove both exclusion
// mechanisms are resolved HOST-SIDE from a subdirectory CHECKS file
// (`config_dir` non-empty) without any `config_dir` field on `CheckInput`.

/// A 6-named-field struct named `Big` with no builder derive — always a violation.
const BIG_STRUCT_SOURCE: &str = r#"pub struct Big {
    a: String,
    b: String,
    c: String,
    d: String,
    e: String,
    f: String,
}
"#;

fn bundled_package(name: &str) -> ExternalCheckPackage {
    use crate::external::{
        BundledExternalCheckPackageProvider, ExternalCheckImplementationRef, ExternalCheckPackageProvider as _,
    };
    BundledExternalCheckPackageProvider
        .resolve(&ExternalCheckImplementationRef::Bundled(name.to_owned()))
        .expect("resolve")
        .expect("bundled package must exist")
}

fn write_file(root: &std::path::Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("rel path has a parent")).expect("create parent dirs");
    fs::write(path, contents).expect("write file");
}

/// Parity round-trip: a qualified `relative/path.rs::Name` exclusion authored in
/// a subdirectory CHECKS file (config_dir = "tools/boss") must exempt exactly
/// that struct in exactly that file when running the real bundled component. A
/// same-named struct in another file is still flagged, and the host resolves the
/// config-relative entry path to repo-relative — all with no `config_dir` on
/// `CheckInput`.
///
/// This is the single real-component parity round-trip for host-side exclusion.
/// The host logic it exercises (`apply_struct_exclusions`, qualified-entry form)
/// is also covered directly by `apply_struct_exclusions_qualified_exempts_only_named_file`.
#[test]
fn giant_structs_qualified_exclusion_exempts_only_the_named_file() {
    let temp = tempdir().expect("temp dir");
    write_file(temp.path(), "tools/boss/sub/types.rs", BIG_STRUCT_SOURCE);
    write_file(temp.path(), "other/types.rs", BIG_STRUCT_SOURCE);

    let tree = LocalSourceTree::new(temp.path()).expect("source tree");
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: PathBuf::from("tools/boss/sub/types.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: PathBuf::from("other/types.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    // Entry path is relative to the CHECKS file's directory (tools/boss).
    let config = toml::Value::Table(toml::toml! {
        exclude_structs = ["sub/types.rs::Big"]
    });

    let executor = crate::external::test_support::executor_with_precompiled_cache(temp.path());
    let result = executor
        .execute(
            &bundled_package("rust/giant-structs"),
            &changeset,
            &tree,
            &config,
            std::path::Path::new("tools/boss"),
            None,
        )
        .expect("execute");

    assert_eq!(
        result.findings.len(),
        1,
        "only the unexcluded same-named struct should remain; got: {:?}",
        result.findings.iter().map(|f| &f.message).collect::<Vec<_>>()
    );
    let loc = result.findings[0].location.as_ref().expect("finding has location");
    assert_eq!(
        loc.path,
        std::path::Path::new("other/types.rs"),
        "the surviving finding must be the struct outside the excluded file"
    );
}

// --- Host-side exclusion mock tests ---
//
// These test the host exclusion functions directly, without spinning up the real
// bundled wasm component, so they run cheaply in `checkleft_lib_test`.

fn make_big_struct_finding(path: &str) -> Finding {
    Finding {
        severity: Severity::Error,
        message: "struct `Big` has more than 5 named fields but lacks `#[derive(..)]`".to_owned(),
        location: Some(Location {
            path: PathBuf::from(path),
            line: Some(1),
            column: None,
        }),
        remediations: vec![],
        suggested_fix: None,
    }
}

/// A simple `Name` exclusion in a subdirectory CHECKS file is scoped to that
/// CHECKS file's subtree: `Big` is exempt for files under `tools/boss` but a
/// same-named struct in `other/types.rs` (outside the subtree) is retained.
/// Exercises `apply_struct_exclusions` directly — no wasm component required.
#[test]
fn apply_struct_exclusions_simple_scopes_to_config_subtree() {
    let mut result = CheckResult {
        check_id: "rust/giant-structs".to_owned(),
        findings: vec![
            make_big_struct_finding("tools/boss/sub/types.rs"),
            make_big_struct_finding("other/types.rs"),
        ],
    };

    let config = toml::Value::Table(toml::toml! {
        exclude_structs = ["Big"]
    });

    apply_struct_exclusions(&mut result, &config, std::path::Path::new("tools/boss"));

    assert_eq!(
        result.findings.len(),
        1,
        "only the struct outside the config subtree should remain"
    );
    assert_eq!(
        result.findings[0].location.as_ref().unwrap().path,
        PathBuf::from("other/types.rs")
    );
}

/// A qualified `path::Name` exclusion is scoped to the exact (repo-relative
/// path, struct-name) pair: it exempts `Big` in `tools/boss/sub/types.rs` but
/// not the same-named struct in `other/types.rs`.
/// Exercises `apply_struct_exclusions` directly — no wasm component required.
#[test]
fn apply_struct_exclusions_qualified_exempts_only_named_file() {
    let mut result = CheckResult {
        check_id: "rust/giant-structs".to_owned(),
        findings: vec![
            make_big_struct_finding("tools/boss/sub/types.rs"),
            make_big_struct_finding("other/types.rs"),
        ],
    };

    let config = toml::Value::Table(toml::toml! {
        exclude_structs = ["sub/types.rs::Big"]
    });

    apply_struct_exclusions(&mut result, &config, std::path::Path::new("tools/boss"));

    assert_eq!(
        result.findings.len(),
        1,
        "only the unexcluded same-named struct should remain"
    );
    assert_eq!(
        result.findings[0].location.as_ref().unwrap().path,
        PathBuf::from("other/types.rs")
    );
}

/// An `exclude_files` glob authored in a subdirectory CHECKS file
/// (config_dir = "sub/dir") is rewritten host-side to a repo-relative glob
/// before the guest sees it. Verifies that `lower_check_input` rewrites `*.rs`
/// to `sub/dir/*.rs` in the serialized config JSON handed to the executor —
/// no wasm component required.
#[test]
fn lower_check_input_scopes_exclude_files_to_config_dir() {
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("sub/dir/inside.rs"),
        kind: ChangeKind::Added,
        old_path: None,
    }]);

    let config = toml::Value::Table(toml::toml! {
        max_lines = 2
        exclude_files = ["*.rs"]
    });

    let input = lower_check_input(&changeset, &NoopSourceTree, &config, std::path::Path::new("sub/dir"))
        .expect("lower_check_input");

    let parsed: serde_json::Value = serde_json::from_str(&input.config_json).expect("parse config_json");
    let exclude_files: Vec<&str> = parsed["exclude_files"]
        .as_array()
        .expect("exclude_files must be an array")
        .iter()
        .map(|v| v.as_str().expect("string element"))
        .collect();
    assert_eq!(
        exclude_files,
        vec!["sub/dir/*.rs"],
        "exclude_files patterns must be rewritten to repo-relative before being handed to the guest"
    );
}

/// An `exclude_structs` entry in a CHECKS config suppresses a
/// `rust/giant-structs-create` finding: the host's `struct_name_from_finding`
/// helper parses the create-check message format (`struct \`Name\` is constructed
/// with …`) correctly, so the named struct is filtered out.
#[test]
fn giant_structs_create_exclude_structs_suppresses_finding() {
    // A 6-field struct literal — triggers rust/giant-structs-create at default threshold.
    const CREATE_VIOLATION: &str = r#"fn make() -> Giant {
    Giant { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 }
}
"#;

    let temp = tempdir().expect("temp dir");
    write_file(temp.path(), "src.rs", CREATE_VIOLATION);

    let tree = LocalSourceTree::new(temp.path()).expect("source tree");
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("src.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    // A simple name exclusion: suppresses the `Giant` struct in the whole subtree.
    let config = toml::Value::Table(toml::toml! {
        exclude_structs = ["Giant"]
    });

    let executor = crate::external::test_support::executor_with_precompiled_cache(temp.path());
    let result = executor
        .execute(
            &bundled_package("rust/giant-structs-create"),
            &changeset,
            &tree,
            &config,
            std::path::Path::new(""),
            None,
        )
        .expect("execute");

    assert!(
        result.findings.is_empty(),
        "exclude_structs = [\"Giant\"] must suppress the create finding; got: {:?}",
        result.findings.iter().map(|f| &f.message).collect::<Vec<_>>()
    );
}
