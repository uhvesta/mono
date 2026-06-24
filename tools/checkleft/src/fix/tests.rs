//! Integration tests for the `checkleft fix` pipeline (T12).
//!
//! These tests prove behavioral safety and correctness properties that span the
//! sandbox, executor, and scheduler:
//!
//! - **Sandbox-escape containment**: a fixer that creates extra files in the
//!   sandbox dir cannot propagate them to the real working tree.
//! - **No-fix-available is a clean no-op**: a package whose invocations carry no
//!   `fix` block produces zero outcomes and leaves originals untouched.
//! - **Atomicity**: a batch fixer that exits non-zero leaves every original file
//!   byte-identical (the sandbox is discarded without copy-back).
//! - **Idempotency**: a second `run_declarative_fix` call on an already-clean tree
//!   produces zero applied files (detect_changes finds no byte difference).
//! - **Deterministic lint-then-format ordering**: the conflict-graph scheduler
//!   always places lint checks before format checks on a shared file so the
//!   formatter normalises whatever lint-fix produced.
//! - **Disjoint checks → concurrent groups**: checks with non-overlapping file sets
//!   produce independent `FixGroup`s that may be applied concurrently.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{TempDir, tempdir};

use crate::external::{
    ExternalCheckDeclarativePackage, ExternalCheckPackageImplementation, parse_declarative_check_manifest,
    run_declarative_fix,
};
use crate::fix::scheduler::build_fix_schedule;
use crate::source_tree::LocalSourceTree;

// ── test helpers ─────────────────────────────────────────────────────────

fn paths(p: &[&str]) -> Vec<PathBuf> {
    p.iter().map(PathBuf::from).collect()
}

fn disk_tree(entries: &[(&str, &[u8])]) -> (TempDir, LocalSourceTree) {
    let dir = tempdir().expect("temp dir");
    for (path, content) in entries {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create dirs");
        }
        fs::write(&full, content).expect("write file");
    }
    let tree = LocalSourceTree::new(dir.path()).expect("create tree");
    (dir, tree)
}

/// Create an executable shell script and return its path.
#[cfg(unix)]
fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
    path
}

/// Build a declarative package whose single invocation has a `fix` block
/// pointing to `binary_path` (called with `{{files}}` in batch mode).
#[cfg(unix)]
fn declarative_with_fixer(binary_path: &str) -> ExternalCheckDeclarativePackage {
    // Double-brace escaping: {{{{files}}}} in format!() → {{files}} in output,
    // which is the template placeholder the declarative executor expects.
    let manifest = format!(
        r#"
id: test/fixer
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**"]

needs:
  fixer:
    default:
      path: "{binary_path}"

invocations:
  - id: check
    run: fixer
    mode: batch
    args: ["{{{{files}}}}"]
    exit:
      "0": ok
      default: error
    transform:
      kind: linelist
      message: "needs fix"
    fix:
      args: ["{{{{files}}}}"]
      exit:
        "0": ok
        default: error
"#
    );
    extract_declarative(parse_declarative_check_manifest(&manifest).expect("parse fixer manifest"))
}

/// Build a declarative package whose invocation has NO `fix` block.
fn declarative_no_fix() -> ExternalCheckDeclarativePackage {
    let manifest = r#"
id: test/no-fix
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**"]

needs:
  checker:
    default:
      path: /bin/true

invocations:
  - id: check
    run: checker
    mode: batch
    args: ["{{files}}"]
    exit:
      "0": ok
      default: error
    transform:
      kind: linelist
      message: "found issue"
"#;
    extract_declarative(parse_declarative_check_manifest(manifest).expect("parse no-fix manifest"))
}

fn extract_declarative(pkg: crate::external::ExternalCheckPackage) -> ExternalCheckDeclarativePackage {
    match pkg.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative package, got {other:?}"),
    }
}

fn empty_config() -> toml::Value {
    toml::Value::Table(Default::default())
}

// ── no-fix-available is a clean no-op ─────────────────────────────────────

/// A package whose invocations declare no `fix` block produces an empty
/// `Vec` from `run_declarative_fix`. This is not an error; it means the
/// check has no automated fix and the user must fix manually.
#[cfg(unix)]
#[test]
fn no_fix_block_returns_no_outcomes_and_leaves_originals_untouched() {
    let (dir, tree) = disk_tree(&[("a.txt", b"needs fixing")]);
    let package = declarative_no_fix();

    let outcomes = run_declarative_fix(dir.path(), &package, &paths(&["a.txt"]), &tree, &empty_config(), |_| {});

    assert!(
        outcomes.is_empty(),
        "a package with no fix block must produce zero outcomes (clean no-op)"
    );
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"needs fixing",
        "real file must be byte-identical when no fix ran"
    );
}

// ── sandbox-escape containment ─────────────────────────────────────────────

/// A fixer may create new files anywhere inside the sandbox directory.
/// Those files are never staged (only files in the fixable set `F` are),
/// so `detect_changes` never sees them and they are never copied back.
/// The real working tree is guaranteed not to receive any unstaged file.
#[cfg(unix)]
#[test]
fn fixer_created_file_outside_staged_set_is_not_propagated_to_real_tree() {
    let (dir, tree) = disk_tree(&[("a.txt", b"before")]);
    let scripts_dir = tempdir().expect("scripts dir");

    // Script fixes staged files AND creates an extra file in the sandbox dir
    // that was never part of the fixable set. The extra file must not escape.
    let script = make_script(
        scripts_dir.path(),
        "escape.sh",
        r#"for f in "$@"; do
  printf 'FIXED' > "$f"
done
# Escape attempt: write a file outside the staged set.
printf 'ESCAPED' > sandbox_escape.txt"#,
    );

    let package = declarative_with_fixer(&script.to_string_lossy());
    let outcomes = run_declarative_fix(dir.path(), &package, &paths(&["a.txt"]), &tree, &empty_config(), |_| {});

    // The staged file was fixed and applied.
    assert_eq!(outcomes.len(), 1, "one invocation outcome expected");
    assert_eq!(
        outcomes[0].applied,
        paths(&["a.txt"]),
        "only the fixable file must be copied back"
    );
    assert!(outcomes[0].error.is_none(), "no invocation error must occur");
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"FIXED",
        "the fixable file must contain the fixed content"
    );

    // The escape file must NOT appear in the real tree.
    assert!(
        !dir.path().join("sandbox_escape.txt").exists(),
        "a file created inside the sandbox but outside the staged set must never appear in the real tree"
    );
}

// ── atomicity: fixer error leaves originals byte-identical ─────────────────

/// When the batch fixer exits with a non-zero code the sandbox is discarded
/// without copy-back. Every original file must be byte-identical afterward.
/// This proves that a fixer failure is structurally atomic: modifying files
/// inside the sandbox does not escape because copy-back never ran.
#[cfg(unix)]
#[test]
fn batch_fixer_error_leaves_all_originals_byte_identical() {
    let (dir, tree) = disk_tree(&[("a.rs", b"fn main() {}"), ("b.rs", b"fn lib() {}")]);
    let scripts_dir = tempdir().expect("scripts dir");

    // Script modifies both files in the sandbox then exits 1 (error).
    // Neither change must reach the real tree.
    let script = make_script(
        scripts_dir.path(),
        "fail.sh",
        r#"for f in "$@"; do
  printf 'MODIFIED' > "$f"
done
exit 1"#,
    );

    let package = declarative_with_fixer(&script.to_string_lossy());
    let outcomes = run_declarative_fix(
        dir.path(),
        &package,
        &paths(&["a.rs", "b.rs"]),
        &tree,
        &empty_config(),
        |_| {},
    );

    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].applied.is_empty(),
        "a batch fixer error must produce zero applied files"
    );
    assert!(
        outcomes[0].error.is_some(),
        "an invocation-level error must be recorded"
    );

    // Originals are byte-identical — the sandbox was discarded without copy-back.
    assert_eq!(
        fs::read(dir.path().join("a.rs")).unwrap(),
        b"fn main() {}",
        "a.rs must be byte-identical after fixer error"
    );
    assert_eq!(
        fs::read(dir.path().join("b.rs")).unwrap(),
        b"fn lib() {}",
        "b.rs must be byte-identical after fixer error"
    );
}

// ── idempotency ────────────────────────────────────────────────────────────

/// A second `run_declarative_fix` call on an already-clean tree produces zero
/// applied files. `detect_changes` re-hashes every staged file after the fixer
/// runs; if the fixer produces the same bytes the pre-fix hash already recorded,
/// the changed set is empty and copy-back writes nothing.
#[cfg(unix)]
#[test]
fn second_fix_pass_on_already_clean_tree_produces_no_writes() {
    let (dir, tree) = disk_tree(&[("a.txt", b"lower")]);
    let scripts_dir = tempdir().expect("scripts dir");

    // Converts file contents to uppercase; already-uppercase content is unchanged.
    let script = make_script(
        scripts_dir.path(),
        "upper.sh",
        r#"for f in "$@"; do
  content=$(cat "$f" | tr 'a-z' 'A-Z')
  printf '%s' "$content" > "$f"
done"#,
    );

    let package = declarative_with_fixer(&script.to_string_lossy());
    let config = empty_config();

    // First pass: "lower" → "LOWER", must be applied.
    let outcomes1 = run_declarative_fix(dir.path(), &package, &paths(&["a.txt"]), &tree, &config, |_| {});
    assert_eq!(outcomes1.len(), 1, "one invocation outcome on first pass");
    assert_eq!(outcomes1[0].applied, paths(&["a.txt"]), "first pass must apply the fix");
    assert!(outcomes1[0].error.is_none(), "no error on first pass");
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"LOWER",
        "file must contain the fixed content after first pass"
    );

    // LocalSourceTree reads from disk on each call, so the same instance
    // sees the updated content for the second staging round.

    // Second pass: "LOWER" → "LOWER" (already uppercase) → detect_changes
    // finds no byte difference → copy-back writes nothing.
    let outcomes2 = run_declarative_fix(dir.path(), &package, &paths(&["a.txt"]), &tree, &config, |_| {});
    assert_eq!(outcomes2.len(), 1, "one invocation outcome on second pass");
    assert!(
        outcomes2[0].applied.is_empty(),
        "second pass on an already-fixed file must produce zero applied files"
    );
    assert!(outcomes2[0].error.is_none(), "no error on second pass");
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"LOWER",
        "file content must be unchanged after idempotent second pass"
    );
}

// ── deterministic lint-then-format ordering ────────────────────────────────

/// When a lint check and a format check both operate on the same file the
/// conflict-graph scheduler produces a single `FixGroup` with lint ordered
/// before format. This ensures the formatter always runs after any rewrite
/// that a lint-fixer may have produced, so the final tree is properly formatted.
#[test]
fn lint_before_format_ordering_for_overlapping_file() {
    let fix_plan: BTreeMap<String, Vec<PathBuf>> = [
        ("lint/oxc".to_owned(), paths(&["src/foo.ts"])),
        ("format/oxc".to_owned(), paths(&["src/foo.ts"])),
    ]
    .into_iter()
    .collect();

    let groups = build_fix_schedule(&fix_plan);

    assert_eq!(
        groups.len(),
        1,
        "overlapping checks must be serialised into a single group"
    );
    assert_eq!(
        groups[0].ordered_checks,
        vec!["lint/oxc", "format/oxc"],
        "lint must be scheduled before format"
    );
}

/// Confirms the same lint-before-format ordering holds for a more complex
/// scenario: three checks sharing two of three files, with one disjoint check.
#[test]
fn lint_before_format_ordering_holds_across_transitive_overlap() {
    // lint/oxc and format/oxc share a.ts.
    // format/rust operates only on b.rs (disjoint from the TS group).
    let fix_plan: BTreeMap<String, Vec<PathBuf>> = [
        ("lint/oxc".to_owned(), paths(&["a.ts"])),
        ("format/oxc".to_owned(), paths(&["a.ts"])),
        ("format/rust".to_owned(), paths(&["b.rs"])),
    ]
    .into_iter()
    .collect();

    let groups = build_fix_schedule(&fix_plan);

    assert_eq!(groups.len(), 2, "disjoint TS and Rust groups must be separate");

    let ts_group = groups
        .iter()
        .find(|g| g.ordered_checks.contains(&"lint/oxc".to_owned()))
        .expect("TS group must exist");
    assert_eq!(
        ts_group.ordered_checks,
        vec!["lint/oxc", "format/oxc"],
        "lint/oxc must precede format/oxc in the TS group"
    );

    let rs_group = groups
        .iter()
        .find(|g| g.ordered_checks.contains(&"format/rust".to_owned()))
        .expect("Rust group must exist");
    assert_eq!(rs_group.ordered_checks, vec!["format/rust"]);
}

// ── disjoint checks → separate concurrent groups ───────────────────────────

/// Checks whose fixable-file sets do not overlap are placed in separate
/// `FixGroup`s. Separate groups are file-disjoint and may be applied
/// concurrently — each group's copy-backs target different real files.
#[test]
fn disjoint_checks_produce_separate_concurrent_groups() {
    let fix_plan: BTreeMap<String, Vec<PathBuf>> = [
        ("format/rust".to_owned(), paths(&["src/a.rs"])),
        ("format/oxc".to_owned(), paths(&["src/b.ts"])),
        ("lint/bazel".to_owned(), paths(&["BUILD.bazel"])),
    ]
    .into_iter()
    .collect();

    let groups = build_fix_schedule(&fix_plan);

    assert_eq!(
        groups.len(),
        3,
        "three fully-disjoint checks must produce three separate groups"
    );

    // Every group has exactly one check.
    for group in &groups {
        assert_eq!(
            group.ordered_checks.len(),
            1,
            "each disjoint group must contain exactly one check"
        );
    }

    // All three check IDs appear exactly once across all groups.
    let all_ids: Vec<&str> = groups
        .iter()
        .flat_map(|g| g.ordered_checks.iter().map(String::as_str))
        .collect();
    assert!(all_ids.contains(&"format/rust"));
    assert!(all_ids.contains(&"format/oxc"));
    assert!(all_ids.contains(&"lint/bazel"));
}

// ── copy-back first-error-stop never half-writes a file ────────────────────

/// When copy-back fails on one file (e.g. a read-only directory), it stops
/// immediately. Files successfully renamed before the error are complete
/// (atomic rename, never partial). The failing target is left untouched.
///
/// Drives `WritableSandbox` directly to inject the I/O failure condition
/// (making a directory read-only) without needing a fixer binary.
#[cfg(unix)]
#[test]
fn copy_back_first_error_stop_never_half_writes_a_file() {
    use std::os::unix::fs::PermissionsExt;

    use crate::external::sandbox::HostCeiling;
    use crate::fix::safety::WritableSandbox;

    let (dir, tree) = disk_tree(&[("x/a.txt", b"aaa"), ("y/b.txt", b"bbb")]);

    // Stage both files and simulate a fixer rewriting them.
    let sandbox =
        WritableSandbox::stage(&paths(&["x/a.txt", "y/b.txt"]), &tree, &HostCeiling::new(dir.path())).expect("stage");
    fs::write(sandbox.root_path().join("x/a.txt"), b"AAA").expect("rewrite a.txt");
    fs::write(sandbox.root_path().join("y/b.txt"), b"BBB").expect("rewrite b.txt");

    // Make directory `y` read-only so the copy-back temp file cannot be created
    // there. This forces an I/O error on the second file's copy-back.
    let y_dir = dir.path().join("y");
    fs::set_permissions(&y_dir, fs::Permissions::from_mode(0o555)).expect("chmod y read-only");

    let report = sandbox.copy_back(&paths(&["x/a.txt", "y/b.txt"]), dir.path());

    // Restore permissions so the TempDir cleanup can remove `y`.
    fs::set_permissions(&y_dir, fs::Permissions::from_mode(0o755)).expect("restore y perms");

    // First file was applied successfully (complete, atomic rename).
    assert_eq!(
        report.applied,
        paths(&["x/a.txt"]),
        "x/a.txt must be applied before the error"
    );
    // Second file's copy-back failed.
    let (failed_path, _) = report.failed.expect("copy-back must record the failure");
    assert_eq!(failed_path, PathBuf::from("y/b.txt"), "failure must name y/b.txt");

    // x/a.txt has the fixed content (atomic rename already completed).
    assert_eq!(
        fs::read(dir.path().join("x/a.txt")).unwrap(),
        b"AAA",
        "successfully applied file must have the fixed content"
    );
    // y/b.txt is untouched (copy-back stopped before writing it).
    assert_eq!(
        fs::read(dir.path().join("y/b.txt")).unwrap(),
        b"bbb",
        "the file whose copy-back failed must retain its original bytes"
    );
}

// ── --allow_dirty filter (via compute_fix_plan contract) ───────────────────
//
// The `--allow_dirty=false` behavior is implemented in `dispatch_fix` via
// `compute_fix_plan`, which partitions failing files into `failing_files`
// (eligible for fixing) and `dirty_skipped` (uncommitted changes, excluded).
//
// Integration-level coverage lives in `src/tests.rs` (the binary-crate tests)
// because `compute_fix_plan` is a private function of `main.rs`. The unit
// tests there cover: dirty partition, all-dirty check still appears, empty
// dirty set does not filter, and the empty-dirty (allow_dirty=true) default.
// This comment documents that T12 acknowledges those tests as the allow_dirty
// proof; no duplication is needed here.
