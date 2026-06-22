//! Tests for the format/oxc declarative check (oxfmt formatter via npm/npx binding).

use std::collections::BTreeMap;
use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{CheckResult, Severity};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode, ToolInvocation};

const FORMAT_OXC_MANIFEST: &str = include_str!("../../../checks/format/oxc.yaml");

fn parse_format_oxc_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(FORMAT_OXC_MANIFEST).expect("format/oxc manifest must parse");
    assert_eq!(package.id, "format/oxc");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

fn tool(invocation: &Invocation) -> &ToolInvocation {
    match &invocation.kind {
        InvocationKind::Tool(tool) => tool,
        other => panic!("expected tool invocation, got {other:?}"),
    }
}

/// Resolved-invocation map for `{{needs.oxfmt.invocation}}` rendering in transform
/// unit tests (the executor builds this from resolved binaries at run time).
fn oxfmt_needs() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("oxfmt".to_owned(), "npx --yes oxfmt@0.55.0".to_owned());
    map
}

#[test]
fn format_oxc_manifest_parses_correctly() {
    let package = parse_format_oxc_package();
    assert!(
        package.skip_symlinks,
        "format/oxc must skip symlinks like prettier/biome"
    );
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    // batch: one oxfmt process over all files, paying npx startup once.
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    // oxfmt exit codes: 0 = all formatted (ok); 1 = some need formatting (findings);
    // 2 = parse/syntax error but oxfmt continues and reports other files (findings);
    // anything else = error so a crash never reads as clean.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn format_oxc_args_use_list_different_and_never_write() {
    let package = parse_format_oxc_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--list-different"),
        "format/oxc must pass --list-different so violated paths appear on stdout; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{files}}"),
        "format/oxc must pass the batch file set via {{{{files}}}}; got: {args:?}"
    );
    // Format-CHECK mode only: must NOT mutate files in the check invocation.
    assert!(
        !args.iter().any(|a| a == "--write" || a == "--fix"),
        "format/oxc check must not write files; got: {args:?}"
    );
}

#[test]
fn format_oxc_needs_npm_default_pinned_to_0_55_0_with_path_fallback() {
    let package = parse_format_oxc_package();
    let req = package.needs.get("oxfmt").expect("oxfmt binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "oxfmt");
            assert_eq!(version, "0.55.0", "default oxfmt version must be 0.55.0");
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
fn format_oxc_applies_to_covers_verified_types_and_excludes_unsupported() {
    use globset::{Glob, GlobSetBuilder};

    let package = parse_format_oxc_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    // The set verified to format correctly + idempotently at the pinned oxfmt 0.55.0.
    for path in [
        "a.js",
        "b.jsx",
        "c.mjs",
        "d.cjs",
        "e.ts",
        "f.tsx",
        "g.mts",
        "h.cts",
        "i.json",
        "j.jsonc",
        "k.json5",
        "l.css",
        "m.scss",
        "n.less",
        "o.html",
        "p.vue",
        "q.md",
        "r.markdown",
        "s.mdx",
        "t.yaml",
        "u.yml",
        "v.toml",
        "w.graphql",
        "x.gql",
    ] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by format/oxc applies_to"
        );
    }

    // oxfmt 0.55.0 does NOT recognise svelte/astro (exits 2), and these are not
    // formattable by it at all — keep them out so the check never lies about scope.
    for path in ["a.svelte", "b.astro", "c.rs", "d.py", "e.png"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by format/oxc applies_to"
        );
    }
}

#[test]
fn format_oxc_npm_default_resolves_to_npx_with_pinned_version() {
    let package = parse_format_oxc_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/bin/npx")),
    )
    .expect("resolve");
    let oxfmt = resolved.get("oxfmt").expect("oxfmt resolved");
    assert_eq!(oxfmt.program, Path::new("/fake/bin/npx"));
    assert_eq!(
        oxfmt.prefix_args,
        vec!["--yes".to_owned(), "oxfmt@0.55.0".to_owned()],
        "default pin must produce `npx --yes oxfmt@0.55.0`"
    );
    assert_eq!(oxfmt.display_invocation, "npx --yes oxfmt@0.55.0");
}

#[test]
fn format_oxc_npm_version_override_repins() {
    let package = parse_format_oxc_package();
    let config: toml::Value = toml::from_str("[needs.oxfmt.npm]\nversion = \"0.56.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["oxfmt"].prefix_args,
        vec!["--yes".to_owned(), "oxfmt@0.56.0".to_owned()],
        "version override must re-pin to 0.56.0 while inheriting the package name"
    );
    assert_eq!(resolved["oxfmt"].display_invocation, "npx --yes oxfmt@0.56.0");
}

#[test]
fn format_oxc_transform_emits_one_file_level_finding_with_dynamic_remediation() {
    let package = parse_format_oxc_package();
    let needs = oxfmt_needs();
    // batch mode: oxfmt --list-different prints the unformatted file's path and
    // exits 1. The linelist transform turns each printed path into one file-level
    // finding; the remediation renders the resolved invocation + the path from
    // stdout as `{{input.file}}`.
    let findings = package.invocations[0]
        .transform
        .apply(b"src/a.ts\n", Some(1), None, Some(&needs))
        .expect("format/oxc transform");
    assert_eq!(findings.len(), 1, "one finding expected; got: {findings:?}");
    let f = &findings[0];
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/a.ts"));
    // Formatting is a whole-file property: no single offending line/column.
    assert_eq!(loc.line, None);
    assert_eq!(loc.column, None);
    assert_eq!(f.severity, Severity::Warning);
    assert!(
        f.message.contains("oxfmt formatting"),
        "message must mention oxfmt formatting; got: {}",
        f.message
    );
    assert_eq!(f.remediations.len(), 1);
    assert!(
        f.remediations[0].contains("npx --yes oxfmt@0.55.0 --write src/a.ts"),
        "remediation must render the dynamic invocation + per-file write command; got: {}",
        f.remediations[0]
    );
}

// ── full executor pipeline against a fake oxfmt on PATH ──────────────────────────

fn format_oxc_manifest_with_path_default(script: &Path) -> String {
    let replaced = FORMAT_OXC_MANIFEST.replace(
        "needs:\n  oxfmt:\n    default:\n      npm:\n        package: \"oxfmt\"\n        version: \"0.55.0\"\n    fallback:\n      path: \"oxfmt\"",
        &format!("needs:\n  oxfmt:\n    default:\n      path: \"{}\"", script.display()),
    );
    assert_ne!(
        replaced, FORMAT_OXC_MANIFEST,
        "path-default replacement must change the manifest"
    );
    replaced
}

#[cfg(unix)]
fn write_executable_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(unix)]
fn run_format_oxc_result(script_body: &str, files: &[&str]) -> anyhow::Result<CheckResult> {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_oxfmt.sh");
    write_executable_script(&script_path, script_body);

    let manifest = format_oxc_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(
        files
            .iter()
            .map(|f| ChangedFile {
                path: std::path::PathBuf::from(f),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    );
    let config = toml::Value::Table(Default::default());
    super::run_declarative_check(temp.path(), "format/oxc", &declarative, &changeset, &config, None)
}

#[cfg(unix)]
fn run_format_oxc(script_body: &str, file: &str) -> CheckResult {
    run_format_oxc_result(script_body, &[file]).expect("run succeeds")
}

#[cfg(unix)]
#[test]
fn format_oxc_end_to_end_reports_unformatted_file() {
    // Fake oxfmt --list-different: print the file path (last arg) and exit 1.
    let result = run_format_oxc(
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\nprintf '%s\\n' \"$last\"\nexit 1\n",
        "src/a.ts",
    );
    assert_eq!(
        result.findings.len(),
        1,
        "expected one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.location.as_ref().expect("location").path, Path::new("src/a.ts"));
    assert_eq!(f.severity, Severity::Warning);
    // The dynamic remediation renders the resolved invocation (here the fake path).
    assert!(
        f.remediations[0].contains("--write src/a.ts"),
        "remediation must render the resolved invocation + per-file write; got: {}",
        f.remediations[0]
    );
}

#[cfg(unix)]
#[test]
fn format_oxc_end_to_end_clean_file_produces_no_findings() {
    let result = run_format_oxc("#!/bin/sh\nexit 0\n", "src/clean.ts");
    assert!(
        result.findings.is_empty(),
        "clean file must produce no findings; got: {:#?}",
        result.findings
    );
}

#[cfg(unix)]
#[test]
fn format_oxc_end_to_end_all_parse_errors_causes_check_error() {
    // oxfmt exits 2 on a parse error. In batch mode, exit 2 maps to `findings`
    // and the linelist transform runs. With empty stdout (all files errored, none
    // unformatted), the linelist bails as an operational error — the check returns
    // Err rather than a spurious "zero findings" result.
    let err = run_format_oxc_result(
        "#!/bin/sh\necho 'x Unexpected token' 1>&2\nexit 2\n",
        &["src/broken.ts"],
    )
    .expect_err("all-parse-error batch must return Err, not Ok");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("operational error") || msg.contains("exit") || msg.contains("no output"),
        "error must explain the cause; got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn format_oxc_end_to_end_batch_reports_multiple_unformatted_files() {
    // Batch mode: fake oxfmt prints all file args that need formatting (all of
    // them here) and exits 1. The linelist transform must produce one finding per
    // path, not just one for the "last" file.
    let result = run_format_oxc_result(
        // Print every argument except the leading --list-different flag
        "#!/bin/sh\nshift\nfor f in \"$@\"; do printf '%s\\n' \"$f\"; done\nexit 1\n",
        &["src/a.ts", "src/b.ts"],
    )
    .expect("batch run succeeds");
    assert_eq!(
        result.findings.len(),
        2,
        "two unformatted files must produce two findings; got: {:#?}",
        result.findings
    );
    let paths: Vec<&Path> = result
        .findings
        .iter()
        .map(|f| f.location.as_ref().expect("location").path.as_path())
        .collect();
    assert!(paths.contains(&Path::new("src/a.ts")));
    assert!(paths.contains(&Path::new("src/b.ts")));
    // Each finding's remediation must name its own specific file.
    for f in &result.findings {
        let file_path = f.location.as_ref().unwrap().path.to_string_lossy();
        assert!(
            f.remediations.iter().any(|r| r.contains(file_path.as_ref())),
            "remediation for {file_path} must reference that file; got: {:?}",
            f.remediations
        );
    }
}

#[cfg(unix)]
#[test]
fn format_oxc_end_to_end_batch_parse_error_in_mixed_batch_continues() {
    // When a batch contains a bad (unparseable) file alongside an unformatted one,
    // oxfmt exits 2 but still prints the unformatted file on stdout. The check must
    // produce a finding for the unformatted file even though one file had an error.
    let result = run_format_oxc_result(
        // Simulate oxfmt: print the last arg (unformatted file) to stdout and exit 2
        // (as if the second arg caused a parse error).
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\nprintf '%s\\n' \"$last\"\nexit 2\n",
        &["src/broken.ts", "src/unformatted.ts"],
    )
    .expect("mixed-error batch still returns Ok (linelist gets a non-empty stdout)");
    assert_eq!(
        result.findings.len(),
        1,
        "mixed batch: only the unformatted file should produce a finding; got: {:#?}",
        result.findings
    );
    assert_eq!(
        result.findings[0].location.as_ref().expect("location").path,
        Path::new("src/unformatted.ts")
    );
}
