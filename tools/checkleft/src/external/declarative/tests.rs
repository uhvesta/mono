//! Tests for the declarative external-check tier.
//!
//! The deterministic tests (always run under `bazel test`) prove **transform-level
//! parity**: real buildifier `--format=json` output, fed through the declarative
//! `json` transform with the spike manifest's `select` + `finding` map, produces
//! exactly the same `Vec<Finding>` as the built-in `BuildifierCheck` parsers.
//!
//! The `e2e_*` tests here are gated behind `CHECKLEFT_SPIKE_E2E=1` because they
//! resolve buildifier via `bazel build` / `bazel cquery`, which cannot run inside
//! the hermetic test sandbox. They are not skipping a failing assertion — they
//! require an external tool — and exercise the production *bazel resolver* path
//! manually.
//!
//! Full **hermetic end-to-end** parity (the real buildifier binary driven through
//! the entire pipeline and compared to the built-in `BuildifierCheck`, under a
//! plain `bazel test`) lives in the sibling [`super::parity_e2e`] module, which
//! gets buildifier from the test's runfiles instead of shelling out to bazel.

use std::path::Path;

use serde_json::Value;

use crate::external::{
    ExternalCheckPackageImplementation, parse_declarative_check_manifest, parse_external_check_package_manifest,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::selector::Selector;
use super::template::{RenderContext, Template};
use super::{ExitOutcome, ExitSemantics, ExternalCheckDeclarativePackage, InvocationMode};

// The committed manifest — the single source of truth for the buildifier
// declarative check definition. Tests source from this file so the test and the
// shipped definition cannot drift.
const BUILDIFIER_MANIFEST: &str = include_str!("../../../checks/buildifier/check.yaml");

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
    let package = parse_declarative_check_manifest(BUILDIFIER_MANIFEST).expect("spike manifest must parse");
    assert_eq!(package.id, "buildifier");
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
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
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
    assert!(format!("{err:#}").contains("unknown binary"), "unexpected: {err:#}");
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
fn declarative_fields_rejected_in_component_mode() {
    let manifest = r#"
id = "x"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "bin/x.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
applies_to = ["**/*.bzl"]
"#;
    let err = parse_external_check_package_manifest(manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("only allowed in `declarative` mode"),
        "unexpected: {err:#}"
    );
}

// ── passthrough transform (the folded `exec` tier) ──────────────────────────────

/// A minimal TOML declarative manifest with a single passthrough invocation. The
/// `tool` binding is filled in by `.replace("TOOL_PATH", …)`. `applies_to = ["**"]`
/// mirrors exactly what the `local_check` bazel rule generates for a folded
/// `exec` binary, so this fixture also guards that codegen's glob choice.
const PASSTHROUGH_MANIFEST: &str = r#"
id = "passthrough-check"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**"]

[needs.tool.default]
path = "TOOL_PATH"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "findings", default = "error" }

[invocations.transform]
kind = "passthrough"
"#;

#[test]
fn manifest_accepts_passthrough_transform() {
    let package = parse_external_check_package_manifest(&PASSTHROUGH_MANIFEST.replace("TOOL_PATH", "emit_findings"))
        .expect("passthrough manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => {
            assert_eq!(declarative.invocations.len(), 1);
            assert_eq!(
                declarative.invocations[0].transform,
                super::transform::Transform::Passthrough
            );
        }
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

#[test]
fn passthrough_transform_rejects_select_and_finding() {
    let manifest = PASSTHROUGH_MANIFEST
        .replace("TOOL_PATH", "emit_findings")
        .replace("kind = \"passthrough\"", "kind = \"passthrough\"\nselect = \".x\"");
    let err = parse_external_check_package_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("must not set `select`"),
        "unexpected: {err:#}"
    );
}

#[test]
fn passthrough_transform_returns_findings_directly() {
    let stdout = br#"{"findings":[
        {"severity":"warning","message":"hello","location":null,"remediations":["fix it"],"suggested_fix":null}
    ]}"#;
    let findings = super::transform::Transform::Passthrough
        .apply(stdout, Some(0), None)
        .expect("passthrough parses findings");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert_eq!(findings[0].message, "hello");
    assert_eq!(findings[0].remediations, vec!["fix it".to_owned()]);
}

#[test]
fn passthrough_transform_surfaces_invalid_json() {
    let err = super::transform::Transform::Passthrough
        .apply(b"not json", Some(0), None)
        .expect_err("invalid findings JSON must error");
    assert!(
        format!("{err:#}").contains("checkleft findings document"),
        "unexpected: {err:#}"
    );
}

/// End-to-end fold of the old `exec` case: a custom binary emits a checkleft
/// findings document on stdout, and the declarative runtime runs it + passes its
/// output through unchanged. Also exercises the relative→null stdin contract
/// (`Command::output` closes stdin, so a `cat`-ing binary sees EOF).
#[test]
#[cfg(unix)]
fn passthrough_runs_binary_end_to_end() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("emit_findings.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"passthrough-ran\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut permissions = std::fs::metadata(&script_path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("chmod");

    let package = parse_external_check_package_manifest(
        &PASSTHROUGH_MANIFEST.replace("TOOL_PATH", &script_path.to_string_lossy()),
    )
    .expect("passthrough manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: "docs/file.md".into(),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let result = super::run_declarative_check(
        temp.path(),
        "passthrough-check",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
    )
    .expect("declarative passthrough runs");

    assert_eq!(result.check_id, "passthrough-check");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Warning);
    assert_eq!(result.findings[0].message, "passthrough-ran");
}

// ── selector unit tests ────────────────────────────────────────────────────────

#[test]
fn selector_filters_files_by_formatted_flag() {
    let selector = Selector::parse(".files[] | select(.formatted == false)").unwrap();
    let root: Value = serde_json::from_slice(REAL_FORMAT_UNFORMATTED).unwrap();
    let rows = selector.select(&root).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("filename").unwrap(), "a/b/unformatted.bzl");

    let clean: Value = serde_json::from_slice(REAL_FORMAT_CLEAN).unwrap();
    assert!(selector.select(&clean).unwrap().is_empty());
}

#[test]
fn selector_flattens_nested_warnings() {
    let selector = Selector::parse(".files[].warnings[]").unwrap();
    let root: Value = serde_json::from_slice(REAL_LINT_WARNINGS).unwrap();
    let rows = selector.select(&root).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("category").unwrap(), "module-docstring");
}

// ── template unit tests ────────────────────────────────────────────────────────

#[test]
fn template_renders_item_and_context_refs() {
    let item: Value = serde_json::json!({"start": {"line": 11}, "category": "no-effect"});
    let context = RenderContext {
        input_file: Some("x/y.bzl"),
        exit_code: Some(0),
    };

    assert_eq!(
        Template::parse("{{item.start.line}}")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "11"
    );
    assert_eq!(
        Template::parse("{{input.file}}")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "x/y.bzl"
    );
    assert_eq!(
        Template::parse("{{item.category}}: hi")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "no-effect: hi"
    );
}

#[test]
fn template_input_file_unavailable_in_batch_errors() {
    let item: Value = serde_json::json!({});
    let context = RenderContext {
        input_file: None,
        exit_code: Some(0),
    };
    let err = Template::parse("{{input.file}}")
        .unwrap()
        .render(&item, context)
        .unwrap_err();
    assert!(format!("{err:#}").contains("per_file mode"));
}

// ── transform-level tests ──────────────────────────────────────────────────────

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
fn format_transform_detects_unformatted_file() {
    let findings = declarative_format_findings(REAL_FORMAT_UNFORMATTED);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(
        findings[0].message.contains("formatting"),
        "expected a formatting message, got: {}",
        findings[0].message
    );
    // Format findings carry no line number (file-level, not line-level).
    assert_eq!(findings[0].location.as_ref().unwrap().line, None);
}

#[test]
fn format_transform_no_finding_on_clean_file() {
    let findings = declarative_format_findings(REAL_FORMAT_CLEAN);
    assert!(findings.is_empty());
}

#[test]
fn lint_transform_produces_one_finding_per_warning() {
    let findings = declarative_lint_findings(REAL_LINT_WARNINGS, "a/b/malformed.bzl");
    assert_eq!(findings.len(), 3);
    // Path comes from invocation context (warnings carry no filename in the JSON).
    assert_eq!(
        findings[0].location.as_ref().unwrap().path,
        Path::new("a/b/malformed.bzl")
    );
    assert_eq!(findings[0].location.as_ref().unwrap().line, Some(11));
    assert_eq!(findings[0].location.as_ref().unwrap().column, Some(1));
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(
        findings[0].message.contains("module-docstring"),
        "first warning should be module-docstring, got: {}",
        findings[0].message
    );
}

#[test]
fn lint_transform_no_findings_on_clean_file() {
    let findings = declarative_lint_findings(REAL_LINT_CLEAN, "a/b/clean.bzl");
    assert!(findings.is_empty());
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
    // Exercises the framework-owned bazel resolver.
    let root = workspace_root();
    let resolved = super::resolve::resolve_bazel_target_executable(&root, "@buildifier_prebuilt//:buildifier")
        .expect("bazel must resolve buildifier");
    assert!(
        resolved.exists(),
        "resolved buildifier path must exist: {}",
        resolved.display()
    );
}

#[test]
fn e2e_declarative_runs_buildifier_end_to_end() {
    if !spike_e2e_enabled() {
        return;
    }
    // Full pipeline: file selection -> binary resolution (path override to the
    // bazel-resolved buildifier) -> invocations -> exit semantics -> transform.
    let root = workspace_root();
    let buildifier = super::resolve::resolve_bazel_target_executable(&root, "@buildifier_prebuilt//:buildifier")
        .expect("resolve buildifier");

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
    std::fs::write(temp.path().join("a/b/malformed.bzl"), FIXTURE).unwrap();
    std::fs::write(temp.path().join("a/b/clean.bzl"), "\"\"\"clean.\"\"\"\n").unwrap();

    let package = parse_package();
    let config: toml::Value =
        toml::from_str(&format!("[needs.buildifier]\npath = \"{}\"\n", buildifier.display())).unwrap();

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

    let result = super::run_declarative_check(temp.path(), "buildifier", &package, &changeset, &config)
        .expect("declarative run");

    // The fixture is format-clean but has 3 lint warnings.
    let lint: Vec<&Finding> = result
        .findings
        .iter()
        .filter(|f| f.location.as_ref().map(|l| l.line.is_some()).unwrap_or(false))
        .collect();
    assert_eq!(lint.len(), 3, "expected 3 lint findings, got {:#?}", result.findings);

    let messages: Vec<&str> = lint.iter().map(|f| f.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("module-docstring")),
        "expected module-docstring; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("unused-variable")),
        "expected unused-variable; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("no-effect")),
        "expected no-effect; got {messages:?}"
    );
}

// ── jaq smoke test ─────────────────────────────────────────────────────────────

/// `empty` is not in jaq_core::core() in 1.x; register it as a native.
fn jaq_empty_run<'a>(
    _: jaq_interpret::Args<'a>,
    _: (jaq_interpret::Ctx<'a>, jaq_interpret::Val),
) -> jaq_interpret::ValRs<'a> {
    Box::new(core::iter::empty::<jaq_interpret::ValR>())
}

/// Prove that jaq-interpret parses and evaluates a filter with no C deps.
#[test]
fn jaq_deps_compile_and_evaluate() {
    use jaq_interpret::{Ctx, FilterT as _, Native, ParseCtx, RcIter, Val};
    use serde_json::json;

    let (stdlib_defs, errs) = jaq_parse::parse("def select(f): if f then . else empty end;", jaq_parse::defs());
    assert!(errs.is_empty(), "stdlib parse errors: {errs:?}");

    let (f, errs) = jaq_parse::parse(".a | select(.b == 1)", jaq_parse::main());
    assert!(errs.is_empty(), "parse errors: {errs:?}");

    let mut ctx = ParseCtx::new(Vec::new());
    ctx.insert_natives(jaq_core::core());
    ctx.insert_native("empty".to_string(), 0, Native::new(jaq_empty_run));
    ctx.insert_defs(stdlib_defs.unwrap_or_default());
    let filter = ctx.compile(f.unwrap());
    assert!(ctx.errs.is_empty(), "compile errors: {} error(s)", ctx.errs.len());

    let inputs = RcIter::new(core::iter::empty());
    let ctx = Ctx::new([], &inputs);
    let input = Val::from(json!({"a": {"b": 1}}));

    let output: Vec<serde_json::Value> = filter
        .run((ctx, input))
        .map(|r| serde_json::Value::from(r.unwrap()))
        .collect();

    assert_eq!(output, vec![json!({"b": 1})]);
}
