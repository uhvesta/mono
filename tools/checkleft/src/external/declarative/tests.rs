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
use crate::output::{CheckResult, Finding, Severity};

use super::selector::Selector;
use super::template::{RenderContext, Template};
use super::{
    ExitOutcome, ExitSemantics, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode,
    ToolInvocation,
};

// The committed manifests — source of truth for the declarative bazel checks.
// Tests source from these files so the tests and shipped definitions cannot drift.
const BUILDIFIER_MANIFEST: &str = include_str!("../../../checks/format/bazel.yaml");
const LINT_BAZEL_MANIFEST: &str = include_str!("../../../checks/lint/bazel.yaml");

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
    let package = parse_declarative_check_manifest(BUILDIFIER_MANIFEST).expect("format/bazel manifest must parse");
    assert_eq!(package.id, "format/bazel");
    assert_eq!(package.runtime, "declarative-v1");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

fn parse_lint_bazel_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(LINT_BAZEL_MANIFEST).expect("lint/bazel manifest must parse");
    assert_eq!(package.id, "lint/bazel");
    assert_eq!(package.runtime, "declarative-v1");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// Unwrap a tool-kind invocation's fields for assertion convenience.
fn tool(invocation: &Invocation) -> &ToolInvocation {
    match &invocation.kind {
        InvocationKind::Tool(tool) => tool,
        other => panic!("expected tool invocation, got {other:?}"),
    }
}

// ── manifest parsing ───────────────────────────────────────────────────────────

#[test]
fn format_bazel_manifest_parses_single_format_invocation() {
    let package = parse_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    assert!(package.needs.contains_key("buildifier"));
    // exit `0 -> findings`, everything else -> error.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn lint_bazel_manifest_parses_single_lint_invocation() {
    let package = parse_lint_bazel_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "lint");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::PerFile);
    assert!(package.needs.contains_key("buildifier"));
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Error);
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
        .apply(stdout, Some(0), None, None)
        .expect("passthrough parses findings");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert_eq!(findings[0].message, "hello");
    assert_eq!(findings[0].remediations, vec!["fix it".to_owned()]);
}

#[test]
fn passthrough_transform_surfaces_invalid_json() {
    let err = super::transform::Transform::Passthrough
        .apply(b"not json", Some(0), None, None)
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
        None,
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
        needs_invocations: None,
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
        needs_invocations: None,
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
        .apply(stdout, Some(0), None, None)
        .expect("format transform")
}

fn declarative_lint_findings(stdout: &[u8], input_file: &str) -> Vec<Finding> {
    let package = parse_lint_bazel_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(0), Some(input_file), None)
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

    let result = super::run_declarative_check(temp.path(), "buildifier", &package, &changeset, &config, None)
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

// ── rustfmt declarative check ──────────────────────────────────────────────────

const RUSTFMT_MANIFEST: &str = include_str!("../../../checks/format/rust.yaml");

fn parse_rustfmt_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(RUSTFMT_MANIFEST).expect("format/rust manifest must parse");
    assert_eq!(package.id, "format/rust");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// Apply the rustfmt linelist transform directly for unit testing.
fn rustfmt_linelist_findings(stdout: &[u8], exit_code: i32) -> Vec<Finding> {
    let package = parse_rustfmt_package();
    assert_eq!(package.invocations.len(), 1);
    package.invocations[0]
        .transform
        .apply(stdout, Some(exit_code), Some("src/lib.rs"), None)
        .expect("rustfmt transform")
}

#[test]
fn rustfmt_manifest_parses_correctly() {
    let package = parse_rustfmt_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::PerFile);
    assert!(package.needs.contains_key("rustfmt"));
    // With --check mode: exit 0 = ok (already formatted), exit 1 = findings
    // (needs formatting, filename on stdout) or operational error (no stdout).
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn rustfmt_config_path_arg_is_present() {
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--config-path={{repo_root}}"),
        "expected --config-path={{{{repo_root}}}} in rustfmt args to pin config to repo root regardless of cwd; got: {args:?}"
    );
}

#[test]
fn rustfmt_check_flag_is_present() {
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--check"),
        "expected --check flag for stable-compatible invocation; got: {args:?}"
    );
}

#[test]
fn rustfmt_list_flag_is_present() {
    // -l prints filenames needing formatting to stdout — required by the linelist
    // transform to distinguish violations (stdout non-empty) from parse errors (empty).
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "-l"),
        "expected -l flag so violated filenames appear on stdout; got: {args:?}"
    );
}

#[test]
fn rustfmt_no_unstable_features_flag() {
    // --unstable-features only exists on nightly rustfmt; stable rejects it.
    // The check must not pass this flag.
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        !args.iter().any(|a| a == "--unstable-features"),
        "--unstable-features must not be in rustfmt args (stable rustfmt rejects it); got: {args:?}"
    );
}

#[test]
fn rustfmt_exit_one_is_findings_not_error() {
    // With --check, exit 1 means the file needs formatting (or a parse error when
    // stdout is empty — the linelist transform handles the distinction).
    let package = parse_rustfmt_package();
    assert_eq!(
        package.invocations[0].exit.classify(Some(1)),
        ExitOutcome::Findings,
        "exit 1 must be classified as findings so the linelist transform runs"
    );
}

#[test]
fn rustfmt_linelist_unformatted_file_produces_finding() {
    // `rustfmt --check -l src/lib.rs` prints the filename when it needs formatting.
    let findings = rustfmt_linelist_findings(b"src/lib.rs\n", 1);
    assert_eq!(findings.len(), 1, "one unformatted file should produce one finding");
    let f = &findings[0];
    assert_eq!(f.severity, Severity::Warning);
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/lib.rs"));
    assert!(loc.line.is_none(), "linelist findings are file-level (no line number)");
    assert!(
        f.message.contains("formatting"),
        "message should mention formatting; got: {}",
        f.message
    );
    assert!(
        f.remediations.iter().any(|r| r.contains("cargo fmt")),
        "remediation should mention cargo fmt; got: {:?}",
        f.remediations
    );
}

#[test]
fn rustfmt_linelist_multiple_files_produce_multiple_findings() {
    let stdout = b"src/a.rs\nsrc/b.rs\n";
    let findings = rustfmt_linelist_findings(stdout, 1);
    assert_eq!(findings.len(), 2, "two unformatted files should produce two findings");
    let paths: Vec<&Path> = findings
        .iter()
        .map(|f| f.location.as_ref().unwrap().path.as_path())
        .collect();
    assert!(paths.contains(&Path::new("src/a.rs")));
    assert!(paths.contains(&Path::new("src/b.rs")));
}

#[test]
fn rustfmt_linelist_nonzero_with_no_output_is_error() {
    // Exit 1 with no stdout = rustfmt parse error (not a formatting violation).
    // The transform must surface this as an error rather than silently returning clean.
    let package = parse_rustfmt_package();
    let err = package.invocations[0]
        .transform
        .apply(b"", Some(1), Some("src/lib.rs"), None)
        .expect_err("empty stdout + exit 1 must be an error, not clean");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("operational error") || msg.contains("parse") || msg.contains("exit"),
        "error must explain the cause; got: {msg}"
    );
}

#[test]
fn rustfmt_has_bazel_default_and_path_fallback() {
    // The manifest must declare a bazel default (hermetic CI toolchain) AND a path
    // fallback (standalone / non-Bazel use). The framework warns loudly when it
    // uses the fallback so the operator knows hermetic resolution was skipped.
    let package = parse_rustfmt_package();
    let req = package.needs.get("rustfmt").expect("rustfmt binary must be declared");
    assert!(
        matches!(req.default, super::BinaryBinding::Bazel(_)),
        "default binding must be bazel for hermetic CI use; got: {:?}",
        req.default
    );
    assert!(
        matches!(req.fallback, Some(super::BinaryBinding::Path(_))),
        "fallback binding must be a path for non-Bazel use; got: {:?}",
        req.fallback
    );
}

#[test]
fn rustfmt_missing_binary_degrades_to_check_error() {
    // When the rustfmt binary cannot be found, run_declarative_check returns
    // Err — the runner converts this to an error-severity finding rather than
    // panicking, so checkleft degrades gracefully.
    use std::path::PathBuf;

    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("write file");

    // Replace the bazel default and remove the fallback so there's no valid binary.
    let manifest = RUSTFMT_MANIFEST.replace(
        "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
        "path: \"nonexistent_rustfmt_binary_xyz\"",
    );
    let package = parse_declarative_check_manifest(&manifest).expect("modified manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("main.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let err = super::run_declarative_check(
        temp.path(),
        "rustfmt",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect_err("missing binary must produce an error, not a successful result");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("nonexistent_rustfmt_binary_xyz") || msg.contains("spawn") || msg.contains("format"),
        "error message should reference the binary or spawn failure; got: {msg}"
    );
}

// ── path normalisation (issue: absolute paths from hermetic toolchain) ──────────

#[test]
fn linelist_absolute_paths_are_normalised_to_repo_relative() {
    // When the hermetic Bazel rustfmt wrapper canonicalises the input path, it
    // echoes back an absolute path. The framework must strip the repo-root prefix
    // before emitting the finding.
    let findings = rustfmt_linelist_findings(b"/repo/root/tools/src/lib.rs\n", 1);
    // Without normalization the path stays absolute — this is what the transform
    // itself returns; normalization is the executor's responsibility.
    assert_eq!(
        findings[0].location.as_ref().unwrap().path,
        Path::new("/repo/root/tools/src/lib.rs"),
        "transform should preserve the path as-is; normalization happens in the executor"
    );

    // Normalization happens inside run_invocation. Verify it via run_declarative_check
    // by wiring up a tiny shell script that prints an absolute path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Fake rustfmt that always exits 1 and prints an absolute path
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\necho '{}/tools/src/lib.rs'\nexit 1\n",
                repo_root.path().display()
            ),
        )
        .expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        let changeset = ChangeSet::new(vec![ChangedFile {
            path: std::path::PathBuf::from("tools/src/lib.rs"),
            kind: crate::input::ChangeKind::Modified,
            old_path: None,
        }]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        assert_eq!(
            result.findings.len(),
            1,
            "expected one finding; got: {:#?}",
            result.findings
        );
        assert_eq!(
            result.findings[0].location.as_ref().unwrap().path,
            Path::new("tools/src/lib.rs"),
            "absolute path must be normalised to repo-relative"
        );
    }
}

// ── duplicate findings (issue: module-tree recursion causes double-reporting) ───

#[test]
fn duplicate_findings_across_module_tree_are_deduplicated() {
    // Simulate rustfmt being invoked per-file for both mod.rs and one of its
    // declared submodule files. When mod.rs recurses into the submodule, both
    // invocations emit a finding for the submodule file — the dedup pass in the
    // executor must collapse them to one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Fake rustfmt: always prints "src/lib.rs" and exits 1, regardless of
        // which file was passed. This simulates rustfmt recursing into a submodule
        // from both the parent and the child invocations.
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'src/lib.rs'\nexit 1\n").expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        // Two files in the changeset — simulating mod.rs + child both in the PR.
        let changeset = ChangeSet::new(vec![
            ChangedFile {
                path: std::path::PathBuf::from("src/mod.rs"),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: std::path::PathBuf::from("src/lib.rs"),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            },
        ]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        assert_eq!(
            result.findings.len(),
            1,
            "duplicate findings for src/lib.rs must be deduplicated to one; got: {:#?}",
            result.findings
        );
        assert_eq!(
            result.findings[0].location.as_ref().unwrap().path,
            Path::new("src/lib.rs")
        );
    }
}

// ── {{repo_root}} arg expansion ───────────────────────────────────────────────────

#[test]
fn rustfmt_repo_root_arg_expands_to_absolute_path() {
    // When run_declarative_check is called, --config-path={{repo_root}} must be
    // expanded to the absolute repo root before rustfmt is invoked. A fake rustfmt
    // script prints its --config-path arg to stdout and exits 1 (→ "findings"); the
    // linelist transform puts each stdout line into a finding's location.path, so we
    // can assert the expanded path was passed to the tool.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Print the --config-path=... arg to stdout and exit 1 (→ findings).
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --config-path=*) echo \"$arg\"; exit 1;;\n  esac\ndone\nexit 0\n",
        )
        .expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        let changeset = crate::input::ChangeSet::new(vec![crate::input::ChangedFile {
            path: std::path::PathBuf::from("src.rs"),
            kind: crate::input::ChangeKind::Modified,
            old_path: None,
        }]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        // The fake script prints exactly one line: --config-path=<path>. The linelist
        // transform records it as finding.location.path (one finding, no line number).
        assert_eq!(
            result.findings.len(),
            1,
            "expected one finding from --config-path echo; got: {:#?}",
            result.findings
        );
        let path_str = result.findings[0].location.as_ref().unwrap().path.to_string_lossy();
        let expected_config_arg = format!("--config-path={}", repo_root.path().display());
        assert_eq!(
            path_str.as_ref(),
            expected_config_arg.as_str(),
            "{{{{repo_root}}}} must expand to the absolute repo root; got: {path_str}"
        );
    }
}

#[test]
fn args_rejects_unknown_template_ref_at_load_time() {
    // An unrecognised {{...}} token in invocation args must be caught at manifest-load
    // time, not silently passed through to the tool.
    let manifest = RUSTFMT_MANIFEST.replace("{{repo_root}}", "{{unknown_var}}");
    let err = parse_declarative_check_manifest(&manifest).expect_err("manifest with {{unknown_var}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown template ref") || msg.contains("unknown_var"),
        "error must name the bad ref; got: {msg}"
    );
}

// ── template validation (issue: {{file}} silently rendered raw) ──────────────────

#[test]
fn linelist_rejects_unknown_template_var_in_remediations_at_parse_time() {
    // {{file}} is not a valid template ref; the check should be rejected at
    // manifest-load time so operators see the error immediately.
    let manifest = RUSTFMT_MANIFEST.replace("{{input.file}}", "{{file}}");
    let err = parse_declarative_check_manifest(&manifest).expect_err("manifest with {{file}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown template ref") || msg.contains("{{file}}"),
        "error must name the bad ref; got: {msg}"
    );
}

#[test]
fn linelist_remediations_substitute_input_file() {
    // {{input.file}} in the remediation text should expand to the file passed to
    // the per-file invocation.
    let findings = rustfmt_linelist_findings(b"src/lib.rs\n", 1);
    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("src/lib.rs"),
        "remediation must contain the input file path `src/lib.rs`; got: {remediation}"
    );
    assert!(
        !remediation.contains("{{"),
        "remediation must not contain unsubstituted template vars; got: {remediation}"
    );
}

// ── bazel_aspect invocation kind ────────────────────────────────────────────────

const CLIPPY_MANIFEST: &str = include_str!("../../../checks/lint/rust.yaml");

fn parse_clippy_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(CLIPPY_MANIFEST).expect("lint/rust manifest must parse");
    assert_eq!(package.id, "lint/rust");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

fn aspect(invocation: &Invocation) -> &super::BazelAspectInvocation {
    match &invocation.kind {
        InvocationKind::BazelAspect(aspect) => aspect,
        other => panic!("expected bazel_aspect invocation, got {other:?}"),
    }
}

#[test]
fn clippy_manifest_parses_as_bazel_aspect() {
    let package = parse_clippy_package();
    assert_eq!(package.invocations.len(), 1);
    let invocation = &package.invocations[0];
    assert_eq!(invocation.id, "clippy");
    let spec = aspect(invocation);
    assert_eq!(spec.aspect, "@rules_rust//rust:defs.bzl%rust_clippy_aspect");
    assert_eq!(spec.output_groups, vec!["clippy_checks".to_owned()]);
    assert_eq!(spec.artifact_format, super::ArtifactFormat::JsonLines);
    // capture_clippy_output is load-bearing: without it the build FAILS on
    // violations instead of writing them to the artifact.
    assert!(
        spec.build_flags
            .iter()
            .any(|f| f.contains("capture_clippy_output=true")),
        "clippy aspect must capture output; got {:?}",
        spec.build_flags
    );
    assert!(
        spec.build_flags.iter().any(|f| f.contains("clippy_error_format=json")),
        "clippy aspect must emit json diagnostics; got {:?}",
        spec.build_flags
    );
    // A clippy-clean build exits 0 and we read (possibly empty) artifacts.
    assert_eq!(invocation.exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(invocation.exit.classify(Some(1)), ExitOutcome::Error);
    // bazel_aspect packages need no binaries: bazel runs the tool.
    assert!(package.needs.is_empty());
}

#[test]
fn clippy_transform_projects_diagnostics_and_skips_summary_rows() {
    let package = parse_clippy_package();
    let invocation = &package.invocations[0];

    // Two real diagnostics + the trailing "warnings emitted" summary row (null
    // code, no spans), as produced by clippy_error_format=json.
    let jsonl = br#"{"$message_type":"diagnostic","message":"unneeded `return` statement","code":{"code":"clippy::needless_return"},"level":"warning","spans":[{"file_name":"lib/rust/git_utils/src/gh_cli.rs","line_start":132,"column_start":5}]}
{"$message_type":"diagnostic","message":"equality checks against true are unnecessary","code":{"code":"clippy::bool_comparison"},"level":"warning","spans":[{"file_name":"lib/rust/git_utils/src/gh_cli.rs","line_start":131,"column_start":8}]}
{"$message_type":"diagnostic","message":"2 warnings emitted","code":null,"level":"warning","spans":[]}"#;

    let document = super::executor::jsonl_to_array(jsonl).expect("valid jsonl");
    let findings = invocation
        .transform
        .apply(&document, Some(0), None, None)
        .expect("transform must project diagnostics");

    assert_eq!(findings.len(), 2, "summary row must be skipped: {findings:?}");
    let first = &findings[0];
    let location = first.location.as_ref().expect("diagnostic finding has a location");
    assert_eq!(location.path, Path::new("lib/rust/git_utils/src/gh_cli.rs"));
    assert_eq!(location.line, Some(132));
    assert_eq!(location.column, Some(5));
    assert!(first.message.contains("clippy::needless_return"));
    assert!(first.message.contains("unneeded `return` statement"));
    assert_eq!(first.severity, Severity::Warning);
    assert!(
        first
            .remediations
            .iter()
            .any(|r| r.contains("#[allow(clippy::needless_return)]")),
        "remediation should name the lint: {:?}",
        first.remediations
    );
}

#[test]
fn jsonl_to_array_rejects_invalid_lines_and_skips_blanks() {
    let array = super::executor::jsonl_to_array(b"{\"a\":1}\n\n{\"b\":2}\n").expect("valid jsonl");
    let value: Value = serde_json::from_slice(&array).unwrap();
    assert_eq!(value.as_array().map(Vec::len), Some(2));

    let err = super::executor::jsonl_to_array(b"{\"a\":1}\nnot-json\n").unwrap_err();
    assert!(
        err.to_string().contains("line 2"),
        "error should name the line: {err:#}"
    );
}

#[test]
fn bazel_aspect_invocation_rejects_tool_fields() {
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
invocations:
  - id: bad
    kind: bazel_aspect
    aspect: "@x//:y.bzl%z"
    output_groups: [g]
    run: sometool
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(err.to_string().contains("must not set `run`"), "got: {err:#}");
}

#[test]
fn bazel_aspect_invocation_requires_aspect_and_output_groups() {
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
invocations:
  - id: bad
    kind: bazel_aspect
    output_groups: [g]
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(err.to_string().contains("must set `aspect`"), "got: {err:#}");

    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
invocations:
  - id: bad
    kind: bazel_aspect
    aspect: "@x//:y.bzl%z"
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(err.to_string().contains("non-empty `output_groups`"), "got: {err:#}");
}

#[test]
fn tool_invocation_rejects_bazel_aspect_fields() {
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
needs:
  t:
    default: {path: t}
invocations:
  - id: bad
    run: t
    mode: batch
    args: ["{{files}}"]
    aspect: "@x//:y.bzl%z"
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(err.to_string().contains("must not set `aspect`"), "got: {err:#}");
}

#[test]
fn tool_invocations_still_require_needs() {
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
invocations:
  - id: bad
    run: t
    mode: batch
    args: ["{{files}}"]
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(
        err.to_string().contains("unknown binary `t`") || err.to_string().contains("must declare at least one binary"),
        "got: {err:#}"
    );
}

// ── per-repo applies_to override via CHECKS.yaml config blob ──────────────────

/// Build a minimal declarative package that matches only `**/*.bzl` files,
/// wired to a shell script that immediately fails (so we can observe whether
/// `run_declarative_check` selected the file at all — if it short-circuits with
/// an empty result it means the file was NOT selected).
#[cfg(unix)]
fn applies_to_test_package(script_path: &str) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test-check"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.bzl"]

[needs.tool.default]
path = "{script_path}"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "findings", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#
    );
    let package = parse_external_check_package_manifest(&manifest).expect("test manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

/// A changeset with one file of each type so we can verify glob selection.
fn changeset_with_files(paths: &[&str]) -> crate::input::ChangeSet {
    crate::input::ChangeSet::new(
        paths
            .iter()
            .map(|p| ChangedFile {
                path: std::path::PathBuf::from(p),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    )
}

#[test]
fn applies_to_override_replaces_definition_glob() {
    // The package applies_to is ["**/*.bzl"]. The config override sets ["**/*.rs"].
    // A changeset with a .rs file should now be selected, while a .bzl file should not.
    let config: toml::Value = toml::from_str(r#"applies_to = ["**/*.rs"]"#).unwrap();
    let globs = super::resolve::override_applies_to(&config)
        .expect("override must be present")
        .expect("override must be valid");
    assert_eq!(globs, vec!["**/*.rs"]);
}

#[test]
fn applies_to_override_absent_falls_back_to_definition() {
    // No `applies_to` key in config → override_applies_to returns None.
    let config: toml::Value = toml::from_str(r#"needs.tool.path = "x""#).unwrap();
    let result = super::resolve::override_applies_to(&config);
    assert!(result.is_none(), "absent override must return None");
}

#[test]
fn applies_to_override_empty_list_is_rejected() {
    let config: toml::Value = toml::from_str("applies_to = []").unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must not be empty"),
        "empty list must be rejected; got: {err:#}"
    );
}

#[test]
fn applies_to_override_non_list_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = "**/*.rs""#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must be a list"),
        "scalar value must be rejected; got: {err:#}"
    );
}

#[test]
fn applies_to_override_empty_string_entry_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = [""]"#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must not be empty"),
        "empty string entry must be rejected; got: {err:#}"
    );
}

/// End-to-end test: config applies_to override restricts file selection so only
/// matching files are checked. The package definition matches `**/*.bzl`; the
/// config override changes it to `**/*.rs`. A .rs file should produce findings;
/// the .bzl file should be skipped (→ empty result, no invocation attempted).
#[test]
#[cfg(unix)]
fn applies_to_override_end_to_end_restricts_selection() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script that emits one finding for any file passed to it.
    let script_path = temp.path().join("emit_one.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"selected\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = applies_to_test_package(&script_path.to_string_lossy());

    // Config override: only match .rs files, not .bzl.
    let config: toml::Value = toml::from_str(r#"applies_to = ["**/*.rs"]"#).unwrap();

    // Changeset has one .rs file and one .bzl file.
    let changeset = changeset_with_files(&["src/main.rs", "BUILD.bzl"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    // The .rs file was selected (→ one finding). The .bzl file was excluded by the override.
    assert_eq!(
        result.findings.len(),
        1,
        "override applies_to must select only .rs file; got: {:#?}",
        result.findings
    );
    assert_eq!(result.findings[0].message, "selected");
}

#[test]
#[cfg(unix)]
fn applies_to_no_override_uses_definition_glob() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script that emits one finding for any file.
    let script_path = temp.path().join("emit_one.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"selected\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = applies_to_test_package(&script_path.to_string_lossy());

    // No applies_to override — definition's ["**/*.bzl"] applies.
    let config: toml::Value = toml::Value::Table(Default::default());
    let changeset = changeset_with_files(&["src/main.rs", "a/b/BUILD.bzl"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    // Only the .bzl file matches; .rs is skipped.
    assert_eq!(
        result.findings.len(),
        1,
        "without override, definition applies_to selects only .bzl; got: {:#?}",
        result.findings
    );
}

#[test]
#[cfg(unix)]
fn applies_to_override_all_files_skipped_returns_empty() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script emits one finding.
    let script_path = temp.path().join("emit_one.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"selected\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = applies_to_test_package(&script_path.to_string_lossy());

    // Override: only frontend/**. Changeset has no frontend files → nothing selected.
    let config: toml::Value = toml::from_str(r#"applies_to = ["frontend/**"]"#).unwrap();
    let changeset = changeset_with_files(&["src/main.rs", "backend/lib.rs"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    assert!(
        result.findings.is_empty(),
        "no files match override glob → no findings; got: {:#?}",
        result.findings
    );
}

// ── batch chunking (ARG_MAX guard) ────────────────────────────────────────────────

#[test]
fn split_files_into_chunks_single_chunk_when_under_threshold() {
    // All files fit comfortably — must produce exactly one chunk.
    let files: Vec<String> = (0..10).map(|i| format!("src/file_{i}.rs")).collect();
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), 10);
}

#[test]
fn split_files_into_chunks_empty_files_returns_one_empty_chunk() {
    let files: Vec<String> = vec![];
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[test]
fn split_files_into_chunks_splits_when_over_threshold() {
    // Use a very small threshold by making the fixed_cost consume almost all of it.
    // Each file "a.rs" costs 5 bytes (4 chars + 1 null). The available budget is
    // ARG_BYTE_SAFE_THRESHOLD - fixed_cost. By setting fixed_cost to
    // ARG_BYTE_SAFE_THRESHOLD - 8, we leave room for 1 file (5 bytes) in the first
    // chunk but not the second (10 bytes total), forcing a split.
    let available_for_files = 8usize; // room for ~1 file of 5 bytes + overflow at 2
    let fixed_cost = super::executor::ARG_BYTE_SAFE_THRESHOLD - available_for_files;
    let files: Vec<String> = vec!["a.rs".to_owned(), "b.rs".to_owned(), "c.rs".to_owned()];
    // Each "a.rs" / "b.rs" / "c.rs" costs 5 bytes. With 8 bytes available:
    //   chunk 1: a.rs (5 bytes used, 3 left; b.rs would need 5 → exceeds 8) → [a.rs]
    //   chunk 2: b.rs (5 bytes) → [b.rs]
    //   chunk 3: c.rs (5 bytes) → [c.rs]
    let chunks = super::executor::split_files_into_chunks(fixed_cost, &files);
    assert!(
        chunks.len() >= 2,
        "files exceeding threshold must be split into multiple chunks; got {} chunk(s)",
        chunks.len()
    );
    // All files must be present across all chunks.
    let all_files: Vec<&String> = chunks.iter().flat_map(|c| c.iter()).collect();
    assert_eq!(all_files.len(), 3);
    assert_eq!(all_files[0], "a.rs");
    assert_eq!(all_files[1], "b.rs");
    assert_eq!(all_files[2], "c.rs");
}

#[test]
fn split_files_into_chunks_single_oversized_file_stays_in_own_chunk() {
    // When a single file alone exceeds the threshold there is no smaller unit;
    // it must still be processed (placed alone in its chunk).
    let file = "x".repeat(super::executor::ARG_BYTE_SAFE_THRESHOLD + 100);
    let files = vec![file.clone()];
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1, "oversized single file must produce exactly one chunk");
    assert_eq!(chunks[0][0], file);
}

/// End-to-end test: a batch invocation over many small files is chunked and
/// findings from all chunks are concatenated. The fake tool emits one finding per
/// file it receives, so the total finding count equals the file count regardless
/// of how many chunks are used.
#[test]
#[cfg(unix)]
fn batch_invocation_chunks_large_file_list_and_concatenates_findings() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script: for each argument that is a known file path, emit one finding.
    // It receives the file list directly as argv entries (no flags before files).
    let script_path = temp.path().join("emit_per_arg.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
findings='{"findings":['
sep=''
for f in "$@"; do
  findings="${findings}${sep}{\"severity\":\"warning\",\"message\":\"found ${f}\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}"
  sep=','
done
findings="${findings}]}"
printf '%s' "$findings"
"#,
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    // Generate enough files with long paths so their combined argv cost exceeds the
    // 128 KB threshold. Each path is ~2600 bytes; 50 of them = ~130 KB, which
    // pushes past 128 KB and forces at least two invocations. The files don't need
    // to exist on disk: select_files matches against the changeset (not the
    // filesystem), and the fake script receives the paths purely as arguments.
    let long_name_prefix = "src/".to_owned() + &"a".repeat(2590);
    let file_names: Vec<String> = (0..50u32).map(|i| format!("{long_name_prefix}{i:04}.rs")).collect();

    let manifest = format!(
        r#"
id = "chunking-test"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.rs"]

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "findings", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#,
        script = script_path.display()
    );

    let package = crate::external::parse_external_check_package_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        crate::external::ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = crate::input::ChangeSet::new(
        file_names
            .iter()
            .map(|name| crate::input::ChangedFile {
                path: std::path::PathBuf::from(name),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    );

    let result = super::run_declarative_check(
        temp.path(),
        "chunking-test",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("chunked batch run must succeed");

    assert_eq!(
        result.findings.len(),
        50,
        "findings from all chunks must be concatenated (one per file); got {:#?}",
        result.findings.len()
    );
}

// ── prettier declarative check + npm version-pinned `needs` binding ──────────────

const PRETTIER_MANIFEST: &str = include_str!("../../../checks/format/prettier.yaml");

fn parse_prettier_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(PRETTIER_MANIFEST).expect("format/prettier manifest must parse");
    assert_eq!(package.id, "format/prettier");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

#[test]
fn prettier_manifest_parses_correctly() {
    let package = parse_prettier_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    // per_file so the linelist remediation can name `{{input.file}}`, mirroring rustfmt.
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::PerFile);
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--list-different"),
        "expected --list-different so violated paths appear on stdout; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--ignore-unknown"),
        "expected --ignore-unknown so unsupported files are skipped, not errors; got: {args:?}"
    );
    // exit 0 = formatted (ok); exit 1 = needs formatting (findings) or operational
    // error (handled by linelist); anything else = error.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn prettier_applies_to_covers_js_ts_and_friends() {
    // Behavioral test: compile the same globset select_files builds and verify
    // representative files match (including tsx/mjs which exercise brace
    // alternation) and that non-prettier file types do not.
    use globset::{Glob, GlobSetBuilder};

    let package = parse_prettier_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    for path in [
        "a.js",
        "b.tsx",
        "c.mjs",
        "d.css",
        "e.json",
        "f.md",
        "g.yaml",
        "h.jsx",
        "i.ts",
        "j.cjs",
        "k.mts",
        "l.cts",
        "m.scss",
        "n.less",
        "o.html",
        "p.vue",
        "q.markdown",
        "r.yml",
        "s.graphql",
        "t.gql",
    ] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by prettier's applies_to"
        );
    }

    for path in ["x.rs", "y.png"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by prettier's applies_to"
        );
    }
}

#[test]
fn prettier_needs_npm_default_pinned_to_3_8_4_with_path_fallback() {
    // The version pin lives in the manifest as the per-check default (3.8.4), with a
    // PATH fallback for environments without npx — mirroring rustfmt's bazel+path shape.
    let package = parse_prettier_package();
    let req = package.needs.get("prettier").expect("prettier binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "prettier");
            assert_eq!(version, "3.8.4", "default Prettier version must be 3.8.4");
        }
        other => panic!("default binding must be an npm version-pinned binding; got: {other:?}"),
    }
    assert!(
        matches!(req.fallback, Some(super::BinaryBinding::Path(_))),
        "fallback binding must be a PATH binary for non-npx environments; got: {:?}",
        req.fallback
    );
}

#[test]
fn npm_default_resolves_to_npx_with_pinned_version_spec() {
    // With npx present, the npm binding resolves to `npx --yes prettier@3.8.4`: the
    // pinned version rides ahead of the check's own args as prefix args.
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let npx = Path::new("/fake/bin/npx");
    let resolved =
        super::resolve::resolve_all_with_npx(Path::new("/repo"), &package.needs, &config, Some(npx)).expect("resolve");
    let prettier = resolved.get("prettier").expect("prettier resolved");
    assert_eq!(prettier.program, npx);
    assert_eq!(
        prettier.prefix_args,
        vec!["--yes".to_owned(), "prettier@3.8.4".to_owned()],
        "default pin must produce `npx --yes prettier@3.8.4`"
    );
}

#[test]
fn npm_version_override_repins_the_pinned_version() {
    // A repo overrides just the version via CHECKS config; the package is inherited
    // from the default npm binding.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier.npm]\nversion = \"3.9.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].prefix_args,
        vec!["--yes".to_owned(), "prettier@3.9.0".to_owned()],
        "version override must re-pin to 3.9.0 while inheriting the package name"
    );
}

#[test]
fn npm_full_override_replaces_package_and_version() {
    let package = parse_prettier_package();
    let config: toml::Value =
        toml::from_str("[needs.prettier.npm]\npackage = \"@scope/prettier\"\nversion = \"4.0.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].prefix_args,
        vec!["--yes".to_owned(), "@scope/prettier@4.0.0".to_owned()]
    );
}

#[test]
fn npm_path_override_swaps_binding_and_drops_npx() {
    // A `path` override fully replaces the npm binding even when npx is available.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier]\npath = \"/opt/prettier\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(resolved["prettier"].program, Path::new("/opt/prettier"));
    assert!(
        resolved["prettier"].prefix_args.is_empty(),
        "a path binding carries no prefix args"
    );
}

#[test]
fn npm_missing_npx_falls_back_to_path_binary() {
    // No npx on PATH: the npm default fails to resolve and the declared `fallback.path`
    // takes over (a loud warning is emitted to stderr).
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let resolved =
        super::resolve::resolve_all_with_npx(Path::new("/repo"), &package.needs, &config, None).expect("resolve");
    assert_eq!(
        resolved["prettier"].program,
        Path::new("prettier"),
        "fallback should use the PATH `prettier` binary"
    );
    assert!(resolved["prettier"].prefix_args.is_empty());
}

#[test]
fn npm_binding_requires_both_package_and_version() {
    let missing_version = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      npm:
        package: "prettier"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(missing_version).expect_err("npm without version must be rejected");
    assert!(err.to_string().contains("must set `version`"), "got: {err:#}");

    let missing_package = missing_version.replace("package: \"prettier\"", "version: \"3.8.4\"");
    let err = parse_declarative_check_manifest(&missing_package).expect_err("npm without package must be rejected");
    assert!(err.to_string().contains("must set `package`"), "got: {err:#}");
}

#[test]
fn binding_rejects_more_than_one_kind() {
    let manifest = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      path: "prettier"
      npm:
        package: "prettier"
        version: "3.8.4"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("two binding kinds must be rejected");
    assert!(
        err.to_string().contains("exactly one of `bazel`, `path`, or `npm`"),
        "got: {err:#}"
    );
}

#[test]
fn path_default_with_fallback_is_rejected() {
    // A `path` default always resolves, so a fallback would be unreachable.
    let manifest = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      path: "prettier"
    fallback:
      path: "prettier2"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("path default + fallback must be rejected");
    assert!(
        err.to_string()
            .contains("fallback is only meaningful when `default` is `bazel` or `npm`"),
        "got: {err:#}"
    );
}

/// Build a prettier manifest whose default binding is a path to a fake script, so
/// the full executor pipeline runs without npx/Node.
fn prettier_manifest_with_path_default(script: &Path) -> String {
    PRETTIER_MANIFEST.replace(
        "needs:\n  prettier:\n    default:\n      npm:\n        package: \"prettier\"\n        version: \"3.8.4\"\n    fallback:\n      path: \"prettier\"",
        &format!("needs:\n  prettier:\n    default:\n      path: \"{}\"", script.display()),
    )
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(unix)]
fn run_prettier_e2e(script_body: &str, file: &str) -> CheckResult {
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("fake_prettier.sh");
    write_executable(&script_path, script_body);

    let manifest = prettier_manifest_with_path_default(&script_path);
    // The replacement must actually change the manifest, else the test would silently
    // exercise the npm binding instead of the fake script.
    assert_ne!(
        manifest, PRETTIER_MANIFEST,
        "path-default replacement did not match the manifest"
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from(file),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    super::run_declarative_check(
        repo_root.path(),
        "format/prettier",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run succeeds")
}

#[cfg(unix)]
#[test]
fn prettier_unformatted_file_produces_finding_with_remediation() {
    // Fake `prettier --list-different ... <file>`: echo the last arg (the file) and
    // exit 1, exactly like prettier listing a file that needs reformatting.
    let result = run_prettier_e2e(
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\"\nexit 1\n",
        "src/app.ts",
    );
    assert_eq!(
        result.findings.len(),
        1,
        "one unformatted file should produce one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.severity, Severity::Warning);
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/app.ts"));
    assert!(loc.line.is_none(), "linelist findings are file-level");
    assert!(
        f.message.contains("prettier formatting"),
        "message should mention prettier; got: {}",
        f.message
    );
    // The e2e helper uses a path-default binding (a fake script), so
    // {{needs.prettier.invocation}} expands to the fake script path rather than
    // `npx --yes prettier@<version>`. Assert the file arg is present; the
    // version-specific assertion is in the dedicated template tests below.
    assert!(
        f.remediations
            .iter()
            .any(|r| r.contains("--write") && r.contains("src/app.ts")),
        "remediation should contain `--write src/app.ts`; got: {:?}",
        f.remediations
    );
    // No unsubstituted template vars must remain.
    assert!(
        f.remediations.iter().all(|r| !r.contains("{{")),
        "remediation must not contain unsubstituted template vars; got: {:?}",
        f.remediations
    );
}

#[cfg(unix)]
#[test]
fn prettier_clean_file_produces_no_finding() {
    // Fake prettier exits 0 (file already formatted) — no findings.
    let result = run_prettier_e2e("#!/bin/sh\nexit 0\n", "src/app.ts");
    assert!(
        result.findings.is_empty(),
        "formatted file should produce no findings; got: {:#?}",
        result.findings
    );
}

#[cfg(unix)]
#[test]
fn prettier_skips_files_outside_applies_to() {
    // A non-prettier file (e.g. a .rs file) must not be selected, so the fake script
    // never runs and there are no findings even though it would exit 1.
    let result = run_prettier_e2e("#!/bin/sh\necho should-not-run\nexit 1\n", "src/lib.rs");
    assert!(
        result.findings.is_empty(),
        "a .rs file is outside prettier's applies_to and must be skipped; got: {:#?}",
        result.findings
    );
}

#[cfg(unix)]
#[test]
fn prettier_exit_two_with_no_output_surfaces_as_error_finding() {
    // prettier exit 2 (e.g. a syntax error) is `default → error` in the manifest.
    // In per_file mode, a file-level error is isolated: it produces an error-severity
    // finding scoped to the file rather than aborting the whole check (which would
    // suppress every other file's results). The check still fails — an error-severity
    // finding is surfaced — but other files are not masked.
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("fake_prettier.sh");
    write_executable(&script_path, "#!/bin/sh\necho 'tool error' >&2\nexit 2\n");
    let manifest = prettier_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/app.ts"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let result = super::run_declarative_check(
        repo_root.path(),
        "format/prettier",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("per_file error mode returns Ok with an error-severity finding, not Err");
    assert_eq!(
        result.findings.len(),
        1,
        "one file → one error finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(
        f.severity,
        Severity::Error,
        "exit-2 file must produce an error-severity finding"
    );
    assert!(
        f.message.contains("exit") || f.message.contains("2"),
        "error finding must explain the failure (exit code); got: {}",
        f.message
    );
    let loc = f.location.as_ref().expect("error finding must name the file");
    assert_eq!(
        loc.path,
        Path::new("src/app.ts"),
        "error finding must be scoped to the failing file"
    );
}

// ── {{needs.<name>.invocation}} template variable ──────────────────────────────

#[test]
fn npm_default_resolved_binary_has_correct_display_invocation() {
    // ResolvedBinary.display_invocation must use "npx --yes <pkg>@<ver>", not the
    // full npx path, so remediation strings are human-readable on any host.
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/usr/bin/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].display_invocation, "npx --yes prettier@3.8.4",
        "default npm binding must produce display_invocation `npx --yes prettier@3.8.4`"
    );
}

#[test]
fn npm_version_override_updates_display_invocation() {
    // When a repo overrides the version to 3.9.0, display_invocation must reflect
    // 3.9.0, not the hardcoded 3.8.4 default. This is the core invariant: the
    // remediation must stay in lockstep with the actual resolved version.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier.npm]\nversion = \"3.9.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/usr/bin/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].display_invocation, "npx --yes prettier@3.9.0",
        "version override to 3.9.0 must update display_invocation to `npx --yes prettier@3.9.0`"
    );
}

#[test]
fn prettier_linelist_remediation_renders_default_invocation() {
    // The {{needs.prettier.invocation}} template must expand to the resolved invocation
    // string. This test drives the linelist transform directly with a pre-populated
    // needs_invocations map, verifying the template expansion without running a binary.
    use std::collections::BTreeMap;
    let package = parse_prettier_package();
    let transform = &package.invocations[0].transform;

    let mut needs_invocations = BTreeMap::new();
    needs_invocations.insert("prettier".to_owned(), "npx --yes prettier@3.8.4".to_owned());

    // Simulate prettier --list-different printing "src/app.ts" and exiting 1.
    let findings = transform
        .apply(b"src/app.ts\n", Some(1), Some("src/app.ts"), Some(&needs_invocations))
        .expect("linelist transform with needs_invocations");

    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("npx --yes prettier@3.8.4 --write src/app.ts"),
        "remediation must expand to the resolved invocation + file; got: {remediation}"
    );
    assert!(
        !remediation.contains("{{"),
        "remediation must not contain unsubstituted template vars; got: {remediation}"
    );
}

#[test]
fn prettier_linelist_remediation_renders_overridden_version() {
    // When the resolved version is 3.9.0, the remediation must say 3.9.0, not 3.8.4.
    // There must be no hardcoded literal — the template must reference the resolved binary.
    use std::collections::BTreeMap;
    let package = parse_prettier_package();
    let transform = &package.invocations[0].transform;

    let mut needs_invocations = BTreeMap::new();
    needs_invocations.insert("prettier".to_owned(), "npx --yes prettier@3.9.0".to_owned());

    let findings = transform
        .apply(b"src/app.ts\n", Some(1), Some("src/app.ts"), Some(&needs_invocations))
        .expect("linelist transform with version override");

    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("npx --yes prettier@3.9.0 --write src/app.ts"),
        "remediation must reflect the overridden version 3.9.0; got: {remediation}"
    );
    assert!(
        !remediation.contains("3.8.4"),
        "remediation must NOT contain the old hardcoded 3.8.4 when version is overridden to 3.9.0; got: {remediation}"
    );
}

// ── skip_symlinks flag ─────────────────────────────────────────────────────────

#[test]
fn prettier_manifest_has_skip_symlinks_true() {
    let package = parse_prettier_package();
    assert!(
        package.skip_symlinks,
        "format/prettier must set skip_symlinks: true so symlinks (e.g. CLAUDE.md -> AGENTS.md) \
         are not passed to prettier, which exits 2 on symlink paths"
    );
}

/// Build a minimal per_file declarative manifest wired to a fake script, with
/// skip_symlinks controlled by the caller. The script always exits 2 (which maps
/// to `default → error`) when invoked, so the test can tell whether the file was
/// selected (error propagated) or skipped (empty result returned early).
#[cfg(unix)]
fn skip_symlinks_package(script: &Path, skip_symlinks: bool) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test-skip-symlinks"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.md"]
skip_symlinks = {skip_symlinks}

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "run"
run = "tool"
mode = "per_file"
args = ["{{{{file}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "hit"
"#,
        script = script.display(),
    );
    let package = parse_external_check_package_manifest(&manifest).expect("test manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn skip_symlinks_true_excludes_symlinked_file() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");

    // Real file.
    std::fs::write(repo_root.path().join("AGENTS.md"), "# Agents\n").expect("write real file");
    // Symlink pointing at the real file (like CLAUDE.md -> AGENTS.md in mono).
    std::os::unix::fs::symlink("AGENTS.md", repo_root.path().join("CLAUDE.md")).expect("create symlink");

    // Script that logs each invocation's file arg, then exits 0 (ok).
    // Verifying CLAUDE.md is absent from the log confirms it was filtered out.
    let script_path2 = repo_root.path().join("count.sh");
    std::fs::write(&script_path2, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms2 = std::fs::metadata(&script_path2).expect("metadata").permissions();
    perms2.set_mode(0o755);
    std::fs::set_permissions(&script_path2, perms2).expect("chmod");

    let package = skip_symlinks_package(&script_path2, true);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("AGENTS.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("CLAUDE.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);
    let result = super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run with skip_symlinks=true must succeed");

    // No findings expected (script exits 0).
    assert!(
        result.findings.is_empty(),
        "skip_symlinks=true with exit-0 script must produce no findings; got: {:#?}",
        result.findings
    );

    // Verify CLAUDE.md was NOT passed to the script by reading the log.
    let log_path = repo_root.path().join("count.sh.log");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("CLAUDE.md"),
        "CLAUDE.md is a symlink and must be skipped with skip_symlinks=true; log: {log}"
    );
    assert!(
        log.contains("AGENTS.md"),
        "AGENTS.md is a real file and must still be checked; log: {log}"
    );
}

#[cfg(unix)]
#[test]
fn skip_symlinks_false_includes_symlinked_file() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");
    std::fs::write(repo_root.path().join("AGENTS.md"), "# Agents\n").expect("write real file");
    std::os::unix::fs::symlink("AGENTS.md", repo_root.path().join("CLAUDE.md")).expect("create symlink");

    let script_path = repo_root.path().join("count.sh");
    std::fs::write(&script_path, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = skip_symlinks_package(&script_path, false);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("AGENTS.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("CLAUDE.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run with skip_symlinks=false must succeed");

    assert!(
        result.findings.is_empty(),
        "exit-0 script must produce no findings; got: {:#?}",
        result.findings
    );

    let log = std::fs::read_to_string(repo_root.path().join("count.sh.log")).unwrap_or_default();
    assert!(
        log.contains("CLAUDE.md"),
        "with skip_symlinks=false, CLAUDE.md (symlink) must still be passed to the tool; log: {log}"
    );
    assert!(
        log.contains("AGENTS.md"),
        "AGENTS.md must be passed to the tool; log: {log}"
    );
}

#[cfg(unix)]
#[test]
fn real_non_symlink_file_always_included_regardless_of_flag() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");
    std::fs::write(repo_root.path().join("README.md"), "# Hello\n").expect("write file");

    let script_path = repo_root.path().join("count.sh");
    std::fs::write(&script_path, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = skip_symlinks_package(&script_path, true);
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("README.md"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run must succeed");

    let log = std::fs::read_to_string(repo_root.path().join("count.sh.log")).unwrap_or_default();
    assert!(
        log.contains("README.md"),
        "README.md is a real file and must be included even with skip_symlinks=true; log: {log}"
    );
}

// ── per_file error isolation ────────────────────────────────────────────────────
//
// These tests verify that a single per_file invocation error does NOT suppress
// other files' findings. One file erroring (default → error) must produce an
// error-severity finding for THAT file and let the loop continue.

/// Build a per_file declarative manifest backed by a fake script, with linelist
/// transform and exit semantics: 0=ok, 1=findings, default=error (so exit 2 = error).
#[cfg(unix)]
fn per_file_error_package(script: &Path) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test/per-file-error"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**"]

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "check"
run = "tool"
mode = "per_file"
args = ["{{{{file}}}}"]
exit = {{ "0" = "ok", "1" = "findings", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "needs formatting"
"#,
        script = script.display(),
    );
    let package = parse_external_check_package_manifest(&manifest).expect("per_file error test manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn per_file_error_isolates_to_file_does_not_abort_check() {
    // Three files: A errors (exit 2), B has findings (exit 1 + stdout), C is clean
    // (exit 0). The error on A must NOT suppress B's findings or C's clean result.
    // After the fix: result contains A's error finding AND B's formatting finding.

    let repo_root = tempfile::tempdir().expect("temp repo root");
    // Script: file_a → exit 2 (error); file_b → print filename + exit 1 (finding);
    // file_c → exit 0 (clean).
    let script_path = repo_root.path().join("per_file_tool.sh");
    write_executable(
        &script_path,
        "#!/bin/sh\ncase \"$1\" in\n  *file_a*) exit 2 ;;\n  *file_b*) echo \"$1\"; exit 1 ;;\n  *) exit 0 ;;\nesac\n",
    );

    let package = per_file_error_package(&script_path);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("file_a.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("file_b.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("file_c.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test/per-file-error",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("per_file error must return Ok with findings, not abort");

    // Expect exactly two findings: an error finding for file_a and a warning finding for file_b.
    assert_eq!(
        result.findings.len(),
        2,
        "expected error finding for file_a + formatting finding for file_b; got: {:#?}",
        result.findings
    );

    let error_findings: Vec<_> = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .collect();
    assert_eq!(
        error_findings.len(),
        1,
        "exactly one error finding (for file_a); got: {:#?}",
        error_findings
    );
    let ef = &error_findings[0];
    assert_eq!(
        ef.location.as_ref().map(|l| l.path.as_path()),
        Some(Path::new("file_a.ts")),
        "error finding must be scoped to file_a"
    );
    assert!(
        ef.message.contains("exit") || ef.message.contains("2"),
        "error finding must mention exit code; got: {}",
        ef.message
    );

    let warning_findings: Vec<_> = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .collect();
    assert_eq!(
        warning_findings.len(),
        1,
        "exactly one warning finding (for file_b); got: {:#?}",
        warning_findings
    );
    assert_eq!(
        warning_findings[0].location.as_ref().map(|l| l.path.as_path()),
        Some(Path::new("file_b.ts")),
        "warning finding must be for file_b"
    );
}

#[cfg(unix)]
#[test]
fn per_file_single_exit2_does_not_hide_other_files_findings() {
    // Regression for the prettier+symlink case: a single file that exits 2 must not
    // mask the findings from other files. Two files: the first exits 2 (error), the
    // second exits 1 with stdout output (finding). The result must contain the
    // formatting finding from the second file.
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("tool.sh");
    // First arg that contains "first" → exit 2; anything else → echo filename + exit 1.
    write_executable(
        &script_path,
        "#!/bin/sh\ncase \"$1\" in\n  *first*) exit 2 ;;\n  *) echo \"$1\"; exit 1 ;;\nesac\n",
    );

    let package = per_file_error_package(&script_path);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("first.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("second.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test/per-file-error",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("per_file error must not abort the check");

    // Both findings must be present: error for first.ts, warning for second.ts.
    assert_eq!(
        result.findings.len(),
        2,
        "exit-2 on first.ts must not mask second.ts's finding; got: {:#?}",
        result.findings
    );

    let has_error_for_first = result.findings.iter().any(|f| {
        f.severity == Severity::Error && f.location.as_ref().map(|l| l.path.as_path()) == Some(Path::new("first.ts"))
    });
    assert!(
        has_error_for_first,
        "error finding for first.ts must be present; got: {:#?}",
        result.findings
    );

    let has_warning_for_second = result.findings.iter().any(|f| {
        f.severity == Severity::Warning && f.location.as_ref().map(|l| l.path.as_path()) == Some(Path::new("second.ts"))
    });
    assert!(
        has_warning_for_second,
        "formatting finding for second.ts must NOT be suppressed by first.ts's error; got: {:#?}",
        result.findings
    );
}
