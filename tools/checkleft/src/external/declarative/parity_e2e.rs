//! Hermetic end-to-end parity test: the **full** declarative pipeline driving a
//! **real** buildifier binary, asserted equal to the built-in [`BuildifierCheck`]
//! on the committed fixtures (`tests/fixtures/buildifier/`).
//!
//! This closes the loop both buildifier spikes left open. The spike-era tests in
//! [`super::tests`] prove *transform-level* parity (canned buildifier JSON →
//! declarative `json` transform == built-in parser) and gate the real-binary
//! tests behind `CHECKLEFT_SPIKE_E2E` because they shell out via `bazel build` /
//! `bazel cquery`, which cannot run inside the hermetic test sandbox. Transform
//! parity alone is not enough: it never exercises file selection, binary
//! resolution, argument templating, the spawn, exit-code classification, or the
//! batch-vs-per-file split. This test runs the entire path.
//!
//! The real buildifier is supplied as a Bazel test `data` dependency
//! (`@buildifier_prebuilt//:buildifier`) and resolved from the test's runfiles —
//! no nested Bazel, no network, fully hermetic. Both the built-in and the
//! declarative check are pointed at that *same* binary (the built-in via
//! `buildifier_path`, the declarative via a `needs.buildifier.path` config
//! override) so the comparison isolates the framework's projection from any
//! difference in the tool itself.
//!
//! # Exit-code semantics (empirically grounded)
//!
//! The manifest classifies exit `0 -> findings`, everything else `-> error`.
//! That is deliberate and load-bearing: buildifier `--format=json` carries its
//! status in the JSON (`"success"`, `"formatted"`, `warnings[]`) and exits `0`
//! for *every* outcome — clean, needs-formatting, and lint warnings alike. Only a
//! genuine failure (an unknown flag, an internal error) exits nonzero, and then
//! it emits no parseable JSON. So `0 -> findings` runs the transform over real
//! output (which naturally yields zero findings when clean) while `default ->
//! error` ensures a crash surfaces as a check error instead of masquerading as
//! "clean". The built-in reaches the same place by ignoring the exit code and
//! reading only the JSON; this test confirms the two strategies agree.

use std::path::{Path, PathBuf};

use crate::check::Check;
use crate::checks::buildifier::BuildifierCheck;
use crate::external::{
    ExternalCheckDeclarativePackage, ExternalCheckPackageImplementation,
    parse_declarative_check_manifest, run_declarative_check,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Finding;
use crate::source_tree::LocalSourceTree;

/// The committed manifest — the canonical first-party buildifier definition.
const MANIFEST: &str = include_str!("../../../checks/buildifier/check.yaml");

/// Format-clean but carries three lint warnings (module-docstring,
/// unused-variable, no-effect). Exercises the per-file lint invocation.
const MALFORMED_FIXTURE: &str =
    include_str!("../../../tests/fixtures/buildifier/malformed.bzl.fixture");

/// Lint-clean but needs reformatting. Exercises the batch format invocation.
const UNFORMATTED_FIXTURE: &str =
    include_str!("../../../tests/fixtures/buildifier/unformatted.bzl.fixture");

/// Resolve the real buildifier binary from the test's runfiles, or `None` when it
/// was not staged (e.g. running under plain `cargo test`).
///
/// Under `bazel test` the `data = ["@buildifier_prebuilt//:buildifier"]` dep
/// always sets `CHECKLEFT_E2E_BUILDIFIER` to the binary's runfiles path, so the
/// assertions below always run in CI. Outside Bazel there are no runfiles, so the
/// test no-ops rather than failing — but we assert we are genuinely outside Bazel
/// (no `TEST_SRCDIR`) so a misconfigured `data`/`env` wiring can never silently
/// skip the parity check in CI.
fn buildifier_from_runfiles() -> Option<PathBuf> {
    match std::env::var("CHECKLEFT_E2E_BUILDIFIER") {
        Ok(rlocationpath) => {
            let runfiles =
                runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let path = runfiles
                .rlocation(&rlocationpath)
                .expect("buildifier rlocation must resolve");
            assert!(
                path.exists(),
                "staged buildifier must exist at {}",
                path.display()
            );
            Some(path)
        }
        Err(_) => {
            assert!(
                std::env::var_os("TEST_SRCDIR").is_none(),
                "running under `bazel test` but CHECKLEFT_E2E_BUILDIFIER is unset — the \
                 buildifier `data`/`env` wiring on checkleft_lib_test is broken; refusing to \
                 silently skip the parity check"
            );
            None
        }
    }
}

/// Run the built-in `BuildifierCheck` over `changeset`, pointed at `buildifier`.
async fn run_builtin(buildifier: &Path, root: &Path, changeset: &ChangeSet) -> Vec<Finding> {
    let mut config = toml::value::Table::new();
    config.insert(
        "buildifier_path".to_owned(),
        toml::Value::String(buildifier.to_string_lossy().into_owned()),
    );
    let tree = LocalSourceTree::new(root).expect("local source tree");
    BuildifierCheck
        .run(changeset, &tree, &toml::Value::Table(config))
        .await
        .expect("built-in buildifier check runs")
        .findings
}

/// Run the declarative check (the committed manifest) over `changeset`, with a
/// `needs.buildifier.path` override pointing at the same `buildifier`.
fn run_declarative(
    buildifier: &Path,
    package: &ExternalCheckDeclarativePackage,
    root: &Path,
    changeset: &ChangeSet,
) -> Vec<Finding> {
    // config = { needs = { buildifier = { path = "<buildifier>" } } }
    let mut path_table = toml::value::Table::new();
    path_table.insert(
        "path".to_owned(),
        toml::Value::String(buildifier.to_string_lossy().into_owned()),
    );
    let mut needs_table = toml::value::Table::new();
    needs_table.insert("buildifier".to_owned(), toml::Value::Table(path_table));
    let mut config = toml::value::Table::new();
    config.insert("needs".to_owned(), toml::Value::Table(needs_table));

    run_declarative_check(
        root,
        "buildifier-declarative",
        package,
        changeset,
        &toml::Value::Table(config),
    )
    .expect("declarative buildifier check runs")
    .findings
}

fn parse_manifest_package() -> ExternalCheckDeclarativePackage {
    match parse_declarative_check_manifest(MANIFEST)
        .expect("committed manifest parses")
        .implementation
    {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// A total order over findings independent of the order each check emits them.
/// The built-in interleaves format+lint per file; the declarative runtime groups
/// all format findings, then all lint findings. The *set* must be identical, so we
/// sort both by (path, line, column, message) before comparing. (path, line,
/// column) alone is ambiguous — malformed.bzl line 11 carries two warnings at
/// different columns — so message is the final tiebreak.
fn sort_key(finding: &Finding) -> (String, u32, u32, String) {
    let location = finding.location.as_ref();
    (
        location
            .map(|l| l.path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        location.and_then(|l| l.line).unwrap_or(0),
        location.and_then(|l| l.column).unwrap_or(0),
        finding.message.clone(),
    )
}

fn sorted(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by_key(sort_key);
    findings
}

/// The full hermetic parity assertion. Materializes both committed fixtures into a
/// temp workspace, runs the built-in and the declarative check over the same
/// buildifier binary, and asserts identical findings.
#[tokio::test]
async fn declarative_matches_builtin_on_committed_fixtures() {
    let Some(buildifier) = buildifier_from_runfiles() else {
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("malformed.bzl"), MALFORMED_FIXTURE).unwrap();
    std::fs::write(temp.path().join("unformatted.bzl"), UNFORMATTED_FIXTURE).unwrap();

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: PathBuf::from("malformed.bzl"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: PathBuf::from("unformatted.bzl"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let package = parse_manifest_package();
    let builtin = run_builtin(&buildifier, temp.path(), &changeset).await;
    let declarative = run_declarative(&buildifier, &package, temp.path(), &changeset);

    // Shape sanity: 3 lint findings (malformed.bzl) + 1 format finding
    // (unformatted.bzl). If buildifier's defaults change this guards against a
    // silently-empty "parity" that would pass vacuously.
    assert_eq!(
        builtin.len(),
        4,
        "expected 4 built-in findings, got {builtin:#?}"
    );
    assert_eq!(
        declarative.len(),
        4,
        "expected 4 declarative findings, got {declarative:#?}"
    );

    assert_eq!(
        sorted(declarative),
        sorted(builtin),
        "declarative pipeline findings must match the built-in BuildifierCheck exactly"
    );
}

/// Single-file parity over only the lint fixture — isolates the per-file lint
/// invocation so a regression there is not masked by the format finding.
#[tokio::test]
async fn declarative_matches_builtin_on_lint_only_fixture() {
    let Some(buildifier) = buildifier_from_runfiles() else {
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("malformed.bzl"), MALFORMED_FIXTURE).unwrap();

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("malformed.bzl"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let package = parse_manifest_package();
    let builtin = run_builtin(&buildifier, temp.path(), &changeset).await;
    let declarative = run_declarative(&buildifier, &package, temp.path(), &changeset);

    assert_eq!(builtin.len(), 3, "expected 3 lint findings, got {builtin:#?}");
    assert_eq!(sorted(declarative), sorted(builtin));
}
