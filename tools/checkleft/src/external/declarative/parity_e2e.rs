//! Hermetic end-to-end correctness test for the declarative buildifier check.
//!
//! Runs the full declarative pipeline — file selection → binary resolution →
//! invocations → exit-code semantics → transforms — against a real buildifier
//! binary staged via Bazel runfiles, and asserts the expected findings against
//! the committed fixtures.
//!
//! The real buildifier is supplied as a Bazel test `data` dependency
//! (`@buildifier_prebuilt//:buildifier`) and resolved from the test's runfiles —
//! no nested Bazel, no network, fully hermetic.
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
//! "clean".

use std::path::{Path, PathBuf};

use crate::external::{
    ExternalCheckDeclarativePackage, ExternalCheckPackageImplementation, parse_declarative_check_manifest,
    run_declarative_check,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Finding;

/// The committed manifest — the canonical first-party buildifier definition.
const MANIFEST: &str = include_str!("../../../checks/buildifier/check.yaml");

/// Format-clean but carries three lint warnings (module-docstring,
/// unused-variable, no-effect). Exercises the per-file lint invocation.
const MALFORMED_FIXTURE: &str = include_str!("../../../tests/fixtures/buildifier/malformed.bzl.fixture");

/// Lint-clean but needs reformatting. Exercises the batch format invocation.
const UNFORMATTED_FIXTURE: &str = include_str!("../../../tests/fixtures/buildifier/unformatted.bzl.fixture");

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
            let runfiles = runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let path = runfiles
                .rlocation(&rlocationpath)
                .expect("buildifier rlocation must resolve");
            assert!(path.exists(), "staged buildifier must exist at {}", path.display());
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

/// Run the declarative check (the committed manifest) over `changeset`, with a
/// `needs.buildifier.path` override pointing at `buildifier`.
fn run_declarative(
    buildifier: &Path,
    package: &ExternalCheckDeclarativePackage,
    root: &Path,
    changeset: &ChangeSet,
) -> Vec<Finding> {
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

/// Full hermetic e2e assertion: materializes both committed fixtures into a temp
/// workspace, runs the declarative check over the staged buildifier binary, and
/// asserts expected finding shape (3 lint findings on malformed.bzl + 1 format
/// finding on unformatted.bzl). If buildifier's defaults change this guards
/// against a silently-empty result that would pass vacuously.
#[tokio::test]
async fn declarative_produces_expected_findings_on_committed_fixtures() {
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
    let findings = sorted(run_declarative(&buildifier, &package, temp.path(), &changeset));

    assert_eq!(
        findings.len(),
        4,
        "expected 4 findings (3 lint on malformed.bzl + 1 format on unformatted.bzl), got {findings:#?}"
    );

    let format_findings: Vec<_> = findings
        .iter()
        .filter(|f| f.location.as_ref().map(|l| l.line.is_none()).unwrap_or(false))
        .collect();
    assert_eq!(format_findings.len(), 1, "expected exactly 1 format finding");
    assert!(
        format_findings[0].message.contains("formatting"),
        "format finding message should mention 'formatting': {}",
        format_findings[0].message
    );

    let lint_findings: Vec<_> = findings
        .iter()
        .filter(|f| f.location.as_ref().map(|l| l.line.is_some()).unwrap_or(false))
        .collect();
    assert_eq!(lint_findings.len(), 3, "expected exactly 3 lint findings");
    let messages: Vec<&str> = lint_findings.iter().map(|f| f.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("module-docstring")),
        "expected a module-docstring warning; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("unused-variable")),
        "expected an unused-variable warning; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("no-effect")),
        "expected a no-effect warning; got {messages:?}"
    );
}

/// Single-file assertion over only the lint fixture — isolates the per-file lint
/// invocation so a regression there is not masked by the format finding.
#[tokio::test]
async fn declarative_produces_expected_lint_findings_on_malformed_fixture() {
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
    let findings = run_declarative(&buildifier, &package, temp.path(), &changeset);

    assert_eq!(findings.len(), 3, "expected 3 lint findings, got {findings:#?}");
}
