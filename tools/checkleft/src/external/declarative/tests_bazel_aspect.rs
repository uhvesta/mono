//! Tests for the `bazel_aspect` invocation kind.

use std::path::Path;

use serde_json::Value;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::output::Severity;

use super::{ArtifactFormat, ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind};

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
    assert_eq!(spec.artifact_format, ArtifactFormat::JsonLines);
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
    // test_rdeps_kinds must be ["rust_test"]: without it, #[cfg(test)] modules are
    // not covered because the lib/binary targets don't compile with --cfg test.
    assert_eq!(
        spec.test_rdeps_kinds,
        vec!["rust_test".to_owned()],
        "lint/rust must set test_rdeps_kinds: [\"rust_test\"] to cover #[cfg(test)] modules"
    );
}

#[test]
fn bazel_aspect_test_rdeps_kinds_defaults_to_empty() {
    // A bazel_aspect invocation without test_rdeps_kinds must default to empty,
    // so no test-rdep extension occurs for aspect checks that don't need it.
    let manifest = r#"
id: test/aspect
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
invocations:
  - id: check
    kind: bazel_aspect
    aspect: "@some//rule:defs.bzl%some_aspect"
    output_groups: [results]
    artifact_format: json
    exit:
      "0": findings
      default: error
    transform:
      kind: passthrough
"#;
    let package = parse_declarative_check_manifest(manifest).expect("manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => {
            let spec = aspect(&d.invocations[0]);
            assert!(
                spec.test_rdeps_kinds.is_empty(),
                "test_rdeps_kinds must default to empty; got {:?}",
                spec.test_rdeps_kinds
            );
        }
        other => panic!("expected declarative, got {other:?}"),
    }
}

#[test]
fn bazel_aspect_test_rdeps_kinds_rejected_on_tool_invocation() {
    let manifest = r#"
id: test/tool
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]
needs:
  mytool:
    default:
      path: /usr/bin/mytool
invocations:
  - id: run
    run: mytool
    mode: batch
    args: ["{{files}}"]
    test_rdeps_kinds: ["rust_test"]
    exit:
      "0": ok
      default: error
    transform:
      kind: passthrough
"#;
    let err = parse_declarative_check_manifest(manifest).unwrap_err();
    assert!(
        err.to_string().contains("test_rdeps_kinds"),
        "error should name the rejected field: {err:#}"
    );
}

#[test]
fn bazel_aspect_non_rust_test_rdeps_kinds_flows_through_to_query() {
    // Verify the generalization: a non-rust aspect with test_rdeps_kinds: ["swift_test"]
    // stores the configured kind in the parsed spec unchanged — no rust hardcoding.
    let manifest = r#"
id: test/swift-aspect
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.swift"]
invocations:
  - id: check
    kind: bazel_aspect
    aspect: "@build_bazel_rules_swift//swift:swift.bzl%swift_lint_aspect"
    output_groups: [lint_results]
    artifact_format: json
    test_rdeps_kinds: ["swift_test"]
    exit:
      "0": findings
      default: error
    transform:
      kind: passthrough
"#;
    let package = parse_declarative_check_manifest(manifest).expect("manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => {
            let spec = aspect(&d.invocations[0]);
            assert_eq!(
                spec.test_rdeps_kinds,
                vec!["swift_test".to_owned()],
                "configured non-rust kind must be stored as-is; no rust hardcoding"
            );
        }
        other => panic!("expected declarative, got {other:?}"),
    }
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
