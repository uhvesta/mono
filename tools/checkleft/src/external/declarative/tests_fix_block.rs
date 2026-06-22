//! Tests for the declarative `fix` block: parse, validate, and backward-compat.

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};

use super::{FixExitOutcome, InvocationMode};

/// A minimal manifest with NO fix block — used to assert fix == None.
const NO_FIX_MANIFEST: &str = r#"
id: format/no-fix
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.txt"]

needs:
  mytool:
    default:
      path: mytool

invocations:
  - id: check
    run: mytool
    mode: batch
    args: ["--check", "{{files}}"]
    exit:
      "0": ok
      "1": findings
      default: error
    transform:
      kind: linelist
      message: "file needs mytool formatting"
"#;

/// A minimal YAML manifest with a `fix` block on a batch invocation.
const FIX_BLOCK_MANIFEST: &str = r#"
id: format/test
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]

needs:
  fmt:
    default:
      path: rustfmt

invocations:
  - id: format
    run: fmt
    mode: batch
    args: ["--check", "{{files}}"]
    exit:
      "0": ok
      "1": findings
      default: error
    transform:
      kind: linelist
      message: "file needs formatting"
    fix:
      args: ["--write", "{{files}}"]
      exit:
        "0": ok
        default: error
"#;

#[test]
fn fix_block_parses_with_inherited_run_and_mode() {
    let package = parse_declarative_check_manifest(FIX_BLOCK_MANIFEST).expect("fix block manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let invocation = &declarative.invocations[0];
    let fix = invocation.fix.as_ref().expect("fix block must be present");

    // `run` inherits from the parent invocation.
    assert_eq!(fix.run, "fmt", "fix.run must inherit parent invocation's run");
    // `mode` inherits from the parent invocation.
    assert_eq!(
        fix.mode,
        InvocationMode::Batch,
        "fix.mode must inherit parent invocation's mode"
    );
    // args are the declared fix args.
    assert_eq!(fix.args, vec!["--write".to_owned(), "{{files}}".to_owned()]);
    // exit semantics: 0 => ok, default => error.
    assert_eq!(fix.exit.classify(Some(0)), FixExitOutcome::Ok);
    assert_eq!(fix.exit.classify(Some(1)), FixExitOutcome::Error);
    assert_eq!(fix.exit.classify(None), FixExitOutcome::Error);
}

#[test]
fn fix_block_absent_means_no_fix() {
    // An invocation without a fix block must have fix == None.
    let package = parse_declarative_check_manifest(NO_FIX_MANIFEST).expect("no-fix manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    assert!(
        declarative.invocations[0].fix.is_none(),
        "invocation without a fix block must have fix == None"
    );
}

#[test]
fn fix_block_default_exit_is_zero_ok_else_error() {
    // When the fix block omits `exit`, the default semantics (0 => ok, else error) apply.
    let manifest = FIX_BLOCK_MANIFEST.replace("      exit:\n        \"0\": ok\n        default: error\n", "");
    let package = parse_declarative_check_manifest(&manifest).expect("fix block without exit must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let fix = declarative.invocations[0].fix.as_ref().expect("fix must be present");
    assert_eq!(
        fix.exit.classify(Some(0)),
        FixExitOutcome::Ok,
        "default: exit 0 must be ok"
    );
    assert_eq!(
        fix.exit.classify(Some(1)),
        FixExitOutcome::Error,
        "default: exit 1 must be error"
    );
    assert_eq!(
        fix.exit.classify(None),
        FixExitOutcome::Error,
        "default: signal must be error"
    );
}

#[test]
fn fix_block_run_override_uses_declared_binary() {
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "    fix:\n      args: [\"--write\", \"{{files}}\"]",
        "    fix:\n      run: fmt\n      args: [\"--write\", \"{{files}}\"]",
    );
    let package = parse_declarative_check_manifest(&manifest).expect("fix with explicit run must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let fix = declarative.invocations[0].fix.as_ref().expect("fix must be present");
    assert_eq!(fix.run, "fmt");
}

#[test]
fn fix_block_mode_override_per_file() {
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "    fix:\n      args: [\"--write\", \"{{files}}\"]",
        "    fix:\n      mode: per_file\n      args: [\"--write\", \"{{file}}\"]",
    );
    let package = parse_declarative_check_manifest(&manifest).expect("fix with per_file mode must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let fix = declarative.invocations[0].fix.as_ref().expect("fix must be present");
    assert_eq!(fix.mode, InvocationMode::PerFile);
    assert_eq!(fix.args, vec!["--write".to_owned(), "{{file}}".to_owned()]);
}

#[test]
fn fix_block_rejects_unknown_run_binary() {
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "    fix:\n      args: [\"--write\", \"{{files}}\"]",
        "    fix:\n      run: nonexistent_tool\n      args: [\"--write\", \"{{files}}\"]",
    );
    let err = parse_declarative_check_manifest(&manifest).expect_err("fix referencing unknown binary must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown binary"),
        "error must mention unknown binary; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_empty_args() {
    let manifest = FIX_BLOCK_MANIFEST.replace("      args: [\"--write\", \"{{files}}\"]", "      args: []");
    let err = parse_declarative_check_manifest(&manifest).expect_err("fix with empty args must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-empty `args`"),
        "error must mention args requirement; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_findings_exit_outcome() {
    let manifest = FIX_BLOCK_MANIFEST.replace("        default: error", "        default: findings");
    let err =
        parse_declarative_check_manifest(&manifest).expect_err("fix with `findings` exit outcome must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("findings") && msg.contains("not valid"),
        "error must explain findings is not valid for fix; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_missing_exit_default() {
    let manifest = FIX_BLOCK_MANIFEST.replace("        default: error\n", "");
    let err = parse_declarative_check_manifest(&manifest).expect_err("fix exit without default must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("default"),
        "error must mention missing default; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_batch_mode_with_file_placeholder() {
    // batch mode must use {{files}}, not {{file}}
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "      args: [\"--write\", \"{{files}}\"]",
        "      args: [\"--write\", \"{{file}}\"]",
    );
    let err = parse_declarative_check_manifest(&manifest).expect_err("batch fix with {{file}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("{{files}}"),
        "error must mention {{files}} requirement; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_per_file_mode_without_file_placeholder() {
    // per_file mode must use {{file}}, not {{files}}
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "    fix:\n      args: [\"--write\", \"{{files}}\"]",
        "    fix:\n      mode: per_file\n      args: [\"--write\", \"{{files}}\"]",
    );
    let err = parse_declarative_check_manifest(&manifest).expect_err("per_file fix without {{file}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("{{file}}"),
        "error must mention {{file}} requirement; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_unknown_template_ref_in_args() {
    let manifest = FIX_BLOCK_MANIFEST.replace(
        "      args: [\"--write\", \"{{files}}\"]",
        "      args: [\"--write\", \"{{files}}\", \"{{unknown}}\"]",
    );
    let err = parse_declarative_check_manifest(&manifest).expect_err("fix with unknown template ref must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown template ref") || msg.contains("unknown"),
        "error must name the bad ref; got: {msg}"
    );
}

#[test]
fn fix_block_rejects_on_bazel_aspect_invocation() {
    let manifest = r#"
id: lint/test
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.rs"]

invocations:
  - id: clippy
    kind: bazel_aspect
    aspect: "@rules_rust//rust:defs.bzl%rust_clippy_aspect"
    output_groups: [clippy_checks]
    exit:
      "0": findings
      default: error
    transform:
      kind: passthrough
    fix:
      args: ["--fix", "{{files}}"]
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("fix block on bazel_aspect must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bazel_aspect") && msg.contains("fix"),
        "error must mention bazel_aspect and fix; got: {msg}"
    );
}

#[test]
fn fix_exit_outcome_ok_and_error_are_accepted() {
    // Both valid fix outcomes must parse without error.
    for outcome in ["ok", "error"] {
        let manifest = FIX_BLOCK_MANIFEST.replace("        \"0\": ok", &format!("        \"0\": {outcome}"));
        parse_declarative_check_manifest(&manifest)
            .unwrap_or_else(|e| panic!("fix outcome `{outcome}` must be valid; got: {e:#}"));
    }
}
