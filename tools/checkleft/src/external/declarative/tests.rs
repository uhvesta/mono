//! Tests for the declarative external-check tier.
//!
//! The deterministic tests (always run under `bazel test`) prove **transform-level
//! parity**: real buildifier `--format=json` output, fed through the declarative
//! `json` transform with the spike manifest's `select` + `finding` map, produces
//! exactly the same `Vec<Finding>` as the built-in `BuildifierCheck` parsers.
//!
//! The `e2e_*` tests are gated behind `CHECKLEFT_SPIKE_E2E=1` because they shell
//! out to a real (bazel-resolved) buildifier, which is not present in the hermetic
//! test sandbox. They are not skipping a failing assertion — they require an
//! external tool — and are run manually for the spike's end-to-end evidence.

use std::path::Path;

use serde_json::Value;

use crate::checks::buildifier::{parse_format_output, parse_lint_output};
use crate::external::{
    ExternalCheckPackageImplementation, parse_declarative_check_manifest,
    parse_external_check_package_manifest,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::selector::Selector;
use super::template::{RenderContext, Template};
use super::{ExternalCheckDeclarativePackage, ExitOutcome, ExitSemantics, InvocationMode};

// The committed manifest — the single source of truth for the buildifier
// declarative check definition. Tests source from this file so the test and the
// shipped definition cannot drift.
const BUILDIFIER_MANIFEST: &str =
    include_str!("../../../checks/buildifier/check.yaml");

// Real buildifier 7.3.1 `--mode=check --format=json` output for an unformatted file.
const REAL_FORMAT_UNFORMATTED: &[u8] =
    br#"{"success":false,"files":[{"filename":"a/b/unformatted.bzl","formatted":false,"valid":true,"warnings":[]}]}"#;

// Real buildifier output for an already-clean file (no format finding).
const REAL_FORMAT_CLEAN: &[u8] =
    br#"{"success":true,"files":[{"filename":"a/b/clean.bzl","formatted":true,"valid":true,"warnings":[]}]}"#;

// Real buildifier `--mode=check --lint=warn --format=json` output for the spike
// fixture (tests/fixtures/buildifier/malformed.bzl.fixture). Note: warnings carry
// NO `filename` — the finding path must come from invocation context.
const REAL_LINT_WARNINGS: &[u8] = br##"{"success":false,"files":[{"filename":"a/b/malformed.bzl","formatted":true,"valid":true,"warnings":[{"start":{"line":11,"column":1},"end":{"line":11,"column":2},"category":"module-docstring","actionable":true,"autoFixable":false,"message":"The file has no module docstring.\nA module docstring is a string literal (not a comment) which should be the first statement of a file (it may follow comment lines).","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#module-docstring"},{"start":{"line":11,"column":19},"end":{"line":11,"column":22},"category":"unused-variable","actionable":true,"autoFixable":false,"message":"Variable \"ctx\" is unused. Please remove it.","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#unused-variable"},{"start":{"line":12,"column":5},"end":{"line":12,"column":24},"category":"no-effect","actionable":true,"autoFixable":false,"message":"Expression result is not used.","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#no-effect"}]}]}"##;

const REAL_LINT_CLEAN: &[u8] =
    br#"{"success":true,"files":[{"filename":"a/b/clean.bzl","formatted":true,"valid":true,"warnings":[]}]}"#;

fn parse_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(BUILDIFIER_MANIFEST)
        .expect("spike manifest must parse");
    assert_eq!(package.id, "buildifier-declarative");
    assert_eq!(package.runtime, "declarative-v1");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

// ── manifest parsing ───────────────────────────────────────────────────────────

#[test]
fn manifest_parses_into_two_invocations() {
    let package = parse_package();
    assert_eq!(package.invocations.len(), 2);
    assert_eq!(package.invocations[0].id, "format");
    assert_eq!(package.invocations[0].mode, InvocationMode::Batch);
    assert_eq!(package.invocations[1].id, "lint");
    assert_eq!(package.invocations[1].mode, InvocationMode::PerFile);
    assert!(package.needs.contains_key("buildifier"));
    // exit `0 -> findings`, everything else -> error.
    assert_eq!(
        package.invocations[0].exit.classify(Some(0)),
        ExitOutcome::Findings
    );
    assert_eq!(
        package.invocations[0].exit.classify(Some(1)),
        ExitOutcome::Error
    );
    assert_eq!(
        package.invocations[0].exit.classify(None),
        ExitOutcome::Error
    );
}

#[test]
fn manifest_rejects_unknown_transform_kind() {
    let manifest = BUILDIFIER_MANIFEST.replace("kind: json", "kind: regex");
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("reserved for a future spike"),
        "unexpected: {err:#}"
    );
}

#[test]
fn manifest_rejects_invocation_with_unknown_binary() {
    let manifest = BUILDIFIER_MANIFEST.replace("run: buildifier", "run: nonexistent");
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("unknown binary"),
        "unexpected: {err:#}"
    );
}

#[test]
fn manifest_requires_default_exit_outcome() {
    // Remove the `default: error` line from the first invocation's exit block.
    let manifest = BUILDIFIER_MANIFEST.replacen("      default: error\n", "", 1);
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("default"),
        "exit semantics must require a default so crashes surface as errors: {err:#}"
    );
}

#[test]
fn declarative_fields_rejected_in_exec_mode() {
    let manifest = r#"
id = "x"
mode = "exec"
runtime = "exec-v1"
api_version = "v1"
executable_path = "bin/x"
applies_to = ["**/*.bzl"]
"#;
    let err = parse_external_check_package_manifest(manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("only allowed in `declarative` mode"),
        "unexpected: {err:#}"
    );
}

// ── selector unit tests ────────────────────────────────────────────────────────

#[test]
fn selector_filters_files_by_formatted_flag() {
    let selector = Selector::parse(".files[] | select(.formatted == false)").unwrap();
    let root: Value = serde_json::from_slice(REAL_FORMAT_UNFORMATTED).unwrap();
    let rows = selector.select(&root);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("filename").unwrap(), "a/b/unformatted.bzl");

    let clean: Value = serde_json::from_slice(REAL_FORMAT_CLEAN).unwrap();
    assert!(selector.select(&clean).is_empty());
}

#[test]
fn selector_flattens_nested_warnings() {
    let selector = Selector::parse(".files[].warnings[]").unwrap();
    let root: Value = serde_json::from_slice(REAL_LINT_WARNINGS).unwrap();
    let rows = selector.select(&root);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("category").unwrap(), "module-docstring");
}

// ── template unit tests ────────────────────────────────────────────────────────

#[test]
fn template_renders_item_and_context_refs() {
    let item: Value = serde_json::json!({"start": {"line": 11}, "category": "no-effect"});
    let context = RenderContext { input_file: Some("x/y.bzl"), exit_code: Some(0) };

    assert_eq!(
        Template::parse("{{item.start.line}}").unwrap().render(&item, context).unwrap(),
        "11"
    );
    assert_eq!(
        Template::parse("{{input.file}}").unwrap().render(&item, context).unwrap(),
        "x/y.bzl"
    );
    assert_eq!(
        Template::parse("{{item.category}}: hi").unwrap().render(&item, context).unwrap(),
        "no-effect: hi"
    );
}

#[test]
fn template_input_file_unavailable_in_batch_errors() {
    let item: Value = serde_json::json!({});
    let context = RenderContext { input_file: None, exit_code: Some(0) };
    let err = Template::parse("{{input.file}}").unwrap().render(&item, context).unwrap_err();
    assert!(format!("{err:#}").contains("per_file mode"));
}

// ── transform-level parity with the built-in BuildifierCheck ────────────────────

fn declarative_format_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(0), None)
        .expect("format transform")
}

fn declarative_lint_findings(stdout: &[u8], input_file: &str) -> Vec<Finding> {
    let package = parse_package();
    package.invocations[1]
        .transform
        .apply(stdout, Some(0), Some(input_file))
        .expect("lint transform")
}

#[test]
fn format_transform_matches_builtin_on_unformatted() {
    let declarative = declarative_format_findings(REAL_FORMAT_UNFORMATTED);
    let builtin = parse_format_output(REAL_FORMAT_UNFORMATTED, Path::new("a/b/unformatted.bzl")).unwrap();
    assert_eq!(declarative, builtin);
    // Spot-check the load-bearing line-less property.
    assert_eq!(declarative.len(), 1);
    assert_eq!(declarative[0].location.as_ref().unwrap().line, None);
}

#[test]
fn format_transform_matches_builtin_on_clean() {
    let declarative = declarative_format_findings(REAL_FORMAT_CLEAN);
    let builtin = parse_format_output(REAL_FORMAT_CLEAN, Path::new("a/b/clean.bzl")).unwrap();
    assert_eq!(declarative, builtin);
    assert!(declarative.is_empty());
}

#[test]
fn lint_transform_matches_builtin_on_warnings() {
    let declarative = declarative_lint_findings(REAL_LINT_WARNINGS, "a/b/malformed.bzl");
    let builtin = parse_lint_output(REAL_LINT_WARNINGS, Path::new("a/b/malformed.bzl")).unwrap();
    assert_eq!(declarative, builtin, "declarative lint findings must match the built-in exactly");
    assert_eq!(declarative.len(), 3);
    // Path comes from invocation context (warnings carry no filename).
    assert_eq!(
        declarative[0].location.as_ref().unwrap().path,
        Path::new("a/b/malformed.bzl")
    );
    assert_eq!(declarative[0].location.as_ref().unwrap().line, Some(11));
    assert_eq!(declarative[0].location.as_ref().unwrap().column, Some(1));
    assert_eq!(declarative[0].severity, Severity::Warning);
}

#[test]
fn lint_transform_matches_builtin_on_clean() {
    let declarative = declarative_lint_findings(REAL_LINT_CLEAN, "a/b/clean.bzl");
    let builtin = parse_lint_output(REAL_LINT_CLEAN, Path::new("a/b/clean.bzl")).unwrap();
    assert_eq!(declarative, builtin);
    assert!(declarative.is_empty());
}

// ── exit semantics: a crash must surface as an error, never silent-clean ────────

#[test]
fn exit_default_error_surfaces_as_transform_error() {
    // Simulate buildifier crashing (nonzero exit, non-JSON stderr-style stdout).
    // The executor's classify maps default -> Error, which aborts with an error;
    // here we assert the model classifies a crash exit as Error rather than Ok.
    let exit = ExitSemantics_for_test();
    assert_eq!(exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(exit.classify(Some(0)), ExitOutcome::Findings);
}

#[allow(non_snake_case)]
fn ExitSemantics_for_test() -> ExitSemantics {
    let package = parse_package();
    package.invocations[0].exit.clone()
}

// ── gated end-to-end against a real buildifier ──────────────────────────────────

// Byte-identical to tests/fixtures/buildifier/malformed.bzl.fixture (inlined so the
// lib test stays hermetic under bazel — include_str! of a non-src file would need
// the fixture added to the test target's compile_data). Line 11 = the `def` (module
// docstring + unused `ctx`), line 12 = the no-effect expression.
const FIXTURE: &str = r#"# This file is intentionally malformed for testing the buildifier check.
#
# buildifier --lint=warn flags it for:
#   - module-docstring: no module-level docstring (must be first statement)
#   - function-docstring: _impl has no docstring
#   - no-effect: the string concatenation on line 12 produces a value that is discarded
#
# buildifier --mode=check also flags the formatting issues below
# (e.g. trailing whitespace, argument style).

def _my_rule_impl(ctx):
    "unused" + "string"
    return []

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {},
)
"#;

fn workspace_root() -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("MODULE.bazel").exists() {
            return dir;
        }
        if !dir.pop() {
            panic!("could not locate MODULE.bazel above CARGO_MANIFEST_DIR");
        }
    }
}

fn spike_e2e_enabled() -> bool {
    std::env::var("CHECKLEFT_SPIKE_E2E").is_ok()
}

#[test]
fn e2e_bazel_resolver_resolves_buildifier() {
    if !spike_e2e_enabled() {
        return;
    }
    // Exercises the framework-owned bazel resolver (reused from the built-in).
    let root = workspace_root();
    let resolved = crate::checks::buildifier::resolve_bazel_target_executable(
        &root,
        "@buildifier_prebuilt//:buildifier",
    )
    .expect("bazel must resolve buildifier");
    assert!(resolved.exists(), "resolved buildifier path must exist: {}", resolved.display());
}

#[test]
fn e2e_declarative_runs_buildifier_end_to_end() {
    if !spike_e2e_enabled() {
        return;
    }
    // Full pipeline: file selection -> binary resolution (path override to the
    // bazel-resolved buildifier) -> invocations -> exit semantics -> transform.
    let root = workspace_root();
    let buildifier = crate::checks::buildifier::resolve_bazel_target_executable(
        &root,
        "@buildifier_prebuilt//:buildifier",
    )
    .expect("resolve buildifier");

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
    std::fs::write(temp.path().join("a/b/malformed.bzl"), FIXTURE).unwrap();
    std::fs::write(temp.path().join("a/b/clean.bzl"), "\"\"\"clean.\"\"\"\n").unwrap();

    let package = parse_package();
    let config: toml::Value = toml::from_str(&format!(
        "[needs.buildifier]\npath = \"{}\"\n",
        buildifier.display()
    ))
    .unwrap();

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: Path::new("a/b/malformed.bzl").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("a/b/clean.bzl").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        temp.path(),
        "buildifier-declarative",
        &package,
        &changeset,
        &config,
    )
    .expect("declarative run");

    // The fixture is format-clean but has 3 lint warnings.
    let lint: Vec<&Finding> = result
        .findings
        .iter()
        .filter(|f| f.location.as_ref().map(|l| l.line.is_some()).unwrap_or(false))
        .collect();
    assert_eq!(lint.len(), 3, "expected 3 lint findings, got {:#?}", result.findings);

    // Parity: the built-in parsers over the same buildifier output.
    let builtin = parse_lint_output(REAL_LINT_WARNINGS, Path::new("a/b/malformed.bzl")).unwrap();
    let categories_builtin: Vec<&str> = builtin.iter().map(|f| f.message.as_str()).collect();
    let categories_declarative: Vec<&str> = lint.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(categories_declarative, categories_builtin);
}
