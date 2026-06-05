//! Buildifier check for checkleft.
//!
//! Runs `buildifier` on each changed Starlark file in the changeset and converts its
//! JSON output to checkleft findings. Requires buildifier 7+ (`--format=json` support).
//! Only files buildifier understands are inspected; unchanged files are never touched.
//!
//! Two passes are run per file: a format pass (`--mode=check`) and a lint pass
//! (`--lint=warn`). Separate invocations give cleaner exit-code semantics — format
//! issues return exit 4, lint warnings return exit 5 — and distinct JSON shapes that
//! are each easier to parse.
//!
//! # Resolving buildifier
//!
//! Two mutually-exclusive config keys control how buildifier is located:
//!
//! - `buildifier_path` — a direct path or binary name on PATH (e.g. `"bin/buildifier"`,
//!   `"buildifier"`). Used as-is; no Bazel involvement.
//!
//! - `buildifier_target` — a Bazel label (e.g. `"@buildifier_prebuilt//:buildifier"`).
//!   checkleft builds the target with `bazel build`, then resolves its executable path
//!   via `bazel cquery --output=starlark`, and execs THAT binary directly — no
//!   `bazel run` and thus no held Bazel lock. The resolved path is cached for the
//!   process lifetime so subsequent files in the same run skip the Bazel overhead.
//!
//! Exactly one key may be set. If neither is configured, `buildifier_target` defaults
//! to `"@buildifier_prebuilt//:buildifier"`, which works out-of-the-box in any Bazel
//! workspace that depends on the `buildifier_prebuilt` module.
//!
//! # Sample CHECKS.yaml entries
//!
//! ```yaml
//! - id: buildifier
//!   # No config needed — defaults to @buildifier_prebuilt//:buildifier via bazel target resolution.
//!
//! - id: buildifier
//!   config:
//!     # Direct path (repobin, PATH, absolute path) — no Bazel required:
//!     buildifier_path: "bin/buildifier"
//!
//! - id: buildifier
//!   config:
//!     # Explicit Bazel target — built and resolved to binary, then exec'd directly:
//!     buildifier_target: "@buildifier_prebuilt//:buildifier"
//! ```
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

// ── public check ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct BuildifierCheck;

#[async_trait]
impl Check for BuildifierCheck {
    fn id(&self) -> &str {
        "buildifier"
    }

    fn description(&self) -> &str {
        "runs buildifier on changed Starlark files, reporting formatting and lint violations"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

// ── config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BuildifierConfigRaw {
    #[serde(default)]
    buildifier_path: Option<String>,
    #[serde(default)]
    buildifier_target: Option<String>,
    #[serde(default = "default_true")]
    check_format: bool,
    #[serde(default = "default_true")]
    check_lint: bool,
}

fn default_true() -> bool {
    true
}

/// How buildifier is located and invoked.
#[derive(Debug)]
enum BuildifierInvocation {
    /// Direct path or binary name — exec'd as-is, no Bazel involved.
    DirectPath(String),
    /// Bazel label — built via `bazel build`, then executable path resolved via
    /// `bazel cquery --output=starlark`, then exec'd directly (no `bazel run`).
    BazelTarget(String),
}

impl BuildifierInvocation {
    fn display_label(&self) -> &str {
        match self {
            Self::DirectPath(p) => p,
            Self::BazelTarget(t) => t,
        }
    }
}

struct BuildifierConfig {
    invocation: BuildifierInvocation,
    check_format: bool,
    check_lint: bool,
    /// Cached resolved binary path — populated at most once per process (first Starlark file
    /// encountered). Avoids re-running `bazel build` + `bazel cquery` on every `run()` call.
    resolved_binary: OnceLock<String>,
}

fn parse_config(config: &toml::Value) -> Result<BuildifierConfig> {
    let raw: BuildifierConfigRaw = config
        .clone()
        .try_into()
        .context("invalid buildifier check config")?;

    let invocation = match (raw.buildifier_path, raw.buildifier_target) {
        (Some(_), Some(_)) => bail!(
            "buildifier check config error: `buildifier_path` and `buildifier_target` are \
             mutually exclusive — set exactly one, or neither (to use the default target \
             `@buildifier_prebuilt//:buildifier`)"
        ),
        (Some(path), None) => BuildifierInvocation::DirectPath(path),
        (None, Some(target)) => BuildifierInvocation::BazelTarget(target),
        (None, None) => {
            BuildifierInvocation::BazelTarget("@buildifier_prebuilt//:buildifier".to_owned())
        }
    };

    Ok(BuildifierConfig {
        invocation,
        check_format: raw.check_format,
        check_lint: raw.check_lint,
        resolved_binary: OnceLock::new(),
    })
}

// ── ConfiguredCheck impl ─────────────────────────────────────────────────────

#[async_trait]
impl ConfiguredCheck for BuildifierConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        if !self.check_format && !self.check_lint {
            return Ok(CheckResult {
                check_id: "buildifier".to_owned(),
                findings: Vec::new(),
            });
        }

        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_buildifier_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };

            // Resolve the binary at most once per process — avoids re-running `bazel build`
            // + `bazel cquery` on every invocation. The `buildifier_path` case is already a
            // direct path; the expense is in BazelTarget resolution.
            // We use get_or_init (stable) by resolving first then storing; a benign race where
            // two threads both resolve results in the same binary path and one value wins.
            let binary_str: &str = if let Some(s) = self.resolved_binary.get() {
                s
            } else {
                let path = resolve_invocation(&self.invocation)?;
                let s = path.to_string_lossy().into_owned();
                self.resolved_binary.get_or_init(|| s)
            };

            if self.check_format {
                match run_format_check(binary_str, &changed_file.path, &contents) {
                    Ok(file_findings) => findings.extend(file_findings),
                    Err(RunError::SpawnFailed(e)) => findings.push(error_finding(
                        &changed_file.path,
                        self.invocation.display_label(),
                        BuildifierError::SpawnFailed(&e),
                    )),
                    Err(RunError::Internal(e)) => {
                        warn!(
                            path = %changed_file.path.display(),
                            error = %e,
                            "buildifier format check internal error"
                        );
                        findings.push(error_finding(
                            &changed_file.path,
                            self.invocation.display_label(),
                            BuildifierError::Internal(&e),
                        ));
                    }
                }
            }

            if self.check_lint {
                match run_lint_check(binary_str, &changed_file.path, &contents) {
                    Ok(file_findings) => findings.extend(file_findings),
                    Err(RunError::SpawnFailed(e)) => findings.push(error_finding(
                        &changed_file.path,
                        self.invocation.display_label(),
                        BuildifierError::SpawnFailed(&e),
                    )),
                    Err(RunError::Internal(e)) => {
                        warn!(
                            path = %changed_file.path.display(),
                            error = %e,
                            "buildifier lint check internal error"
                        );
                        findings.push(error_finding(
                            &changed_file.path,
                            self.invocation.display_label(),
                            BuildifierError::Internal(&e),
                        ));
                    }
                }
            }
        }

        Ok(CheckResult {
            check_id: "buildifier".to_owned(),
            findings,
        })
    }
}

/// Resolves the buildifier invocation to an executable path.
///
/// For `DirectPath`, returns the path as-is (may be a name on PATH or an absolute path).
/// For `BazelTarget`, builds the target and resolves its executable via `bazel cquery`.
fn resolve_invocation(invocation: &BuildifierInvocation) -> Result<PathBuf> {
    match invocation {
        BuildifierInvocation::DirectPath(path) => Ok(PathBuf::from(path)),
        BuildifierInvocation::BazelTarget(target) => {
            let repo_root = std::env::current_dir()
                .context("failed to get current working directory for bazel target resolution")?;
            resolve_bazel_target_executable(&repo_root, target)
        }
    }
}

/// Builds `target` and resolves its executable path via `bazel cquery --output=starlark`.
/// Returns an absolute path to the built binary.
///
/// `pub(crate)` so the framework-owned declarative external-check resolver can reuse
/// the exact same Bazel resolution the built-in buildifier check uses.
pub(crate) fn resolve_bazel_target_executable(repo_root: &Path, target: &str) -> Result<PathBuf> {
    // Build the target first.
    let build_output = Command::new("bazel")
        .arg("build")
        .arg("--color=no")
        .arg("--curses=no")
        .arg("--noshow_progress")
        .arg("--show_result=0")
        .arg("--ui_event_filters=-info")
        .arg(target)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to spawn `bazel build {target}`"))?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        bail!(
            "`bazel build {target}` failed (exit {}): {}",
            build_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    // Resolve the executable path via cquery.
    let cquery_output = Command::new("bazel")
        .arg("cquery")
        .arg("--color=no")
        .arg("--curses=no")
        .arg("--noshow_progress")
        .arg(target)
        .arg("--output=starlark")
        .arg("--starlark:expr=target.files_to_run.executable.path if target.files_to_run.executable else ''")
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to spawn `bazel cquery {target}`"))?;

    if !cquery_output.status.success() {
        let stderr = String::from_utf8_lossy(&cquery_output.stderr);
        bail!(
            "`bazel cquery {target}` failed (exit {}): {}",
            cquery_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let raw = String::from_utf8_lossy(&cquery_output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();

    if raw.is_empty() {
        bail!(
            "bazel target `{target}` does not produce an executable \
             (cquery returned an empty path)"
        );
    }

    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_root.join(path))
    }
}

/// Represents the kind of failure that occurred when running buildifier.
enum BuildifierError<'a> {
    /// buildifier could not be spawned (binary not found, not executable, etc.).
    SpawnFailed(&'a anyhow::Error),
    /// buildifier ran but checkleft encountered an internal error (e.g. JSON parse failure).
    Internal(&'a anyhow::Error),
}

fn error_finding(file_path: &Path, display_label: &str, err: BuildifierError<'_>) -> Finding {
    match err {
        BuildifierError::SpawnFailed(e) => Finding {
            severity: Severity::Warning,
            message: format!("could not run buildifier on `{}`: {e}", file_path.display()),
            location: Some(Location {
                path: file_path.to_path_buf(),
                line: None,
                column: None,
            }),
            remediations: vec![format!(
                "Ensure buildifier is installed and reachable as `{display_label}`."
            )],
            suggested_fix: None,
        },
        BuildifierError::Internal(e) => Finding {
            severity: Severity::Warning,
            message: format!(
                "could not run buildifier on `{}`: {e}",
                file_path.display()
            ),
            location: Some(Location {
                path: file_path.to_path_buf(),
                line: None,
                column: None,
            }),
            remediations: vec![
                "This is an internal checkleft error, not an environment problem. \
                 Please file a bug against checkleft."
                    .to_owned(),
            ],
            suggested_fix: None,
        },
    }
}

// ── file-kind filter ──────────────────────────────────────────────────────────

/// Returns `true` for file names / extensions that buildifier processes.
pub(crate) fn is_buildifier_file(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("BUILD" | "BUILD.bazel" | "MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel") => true,
        _ => matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("bzl" | "star")
        ),
    }
}

// ── buildifier invocations ────────────────────────────────────────────────────

/// Classifies a buildifier run error so callers can produce the right finding.
enum RunError {
    /// buildifier binary could not be spawned — the user's environment is likely misconfigured.
    SpawnFailed(anyhow::Error),
    /// buildifier ran, but checkleft encountered an unexpected internal error.
    Internal(anyhow::Error),
}

/// Runs the format pass (`--mode=check --format=json`) and returns a finding if the file
/// needs reformatting.
fn run_format_check(
    binary: &str,
    file_path: &Path,
    contents: &[u8],
) -> std::result::Result<Vec<Finding>, RunError> {
    let path_flag = format!("-path={}", file_path.to_string_lossy());
    let output = invoke_buildifier(
        binary,
        &["--mode=check", "--format=json", &path_flag, "-"],
        contents,
    )?;
    parse_format_output(&output.stdout, file_path).map_err(RunError::Internal)
}

/// Runs the lint pass (`--mode=check --lint=warn --format=json`) and returns one finding per warning.
///
/// `--format=json` requires `--mode=check`; without it buildifier exits with an error.
fn run_lint_check(
    binary: &str,
    file_path: &Path,
    contents: &[u8],
) -> std::result::Result<Vec<Finding>, RunError> {
    let path_flag = format!("-path={}", file_path.to_string_lossy());
    let output = invoke_buildifier(
        binary,
        &["--mode=check", "--lint=warn", "--format=json", &path_flag, "-"],
        contents,
    )?;
    parse_lint_output(&output.stdout, file_path).map_err(RunError::Internal)
}

/// Spawns buildifier at `binary` with `buildifier_args` and pipes `contents` to its stdin.
fn invoke_buildifier(
    binary: &str,
    buildifier_args: &[&str],
    contents: &[u8],
) -> std::result::Result<Output, RunError> {
    if binary.is_empty() {
        return Err(RunError::Internal(anyhow::anyhow!(
            "buildifier binary path is empty"
        )));
    }

    let mut cmd = Command::new(binary);
    cmd.args(buildifier_args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        RunError::SpawnFailed(
            anyhow::Error::new(e).context(format!("failed to spawn buildifier `{binary}`")),
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(contents)
            .map_err(|e| RunError::Internal(anyhow::Error::new(e).context("failed to write to buildifier stdin")))?;
    }

    child
        .wait_with_output()
        .map_err(|e| RunError::Internal(anyhow::Error::new(e).context("failed to wait for buildifier")))
}

// ── JSON output parsing ───────────────────────────────────────────────────────

/// Parses `--mode=check --format=json` output and returns a finding if the file is
/// not formatted.
pub(crate) fn parse_format_output(stdout: &[u8], file_path: &Path) -> Result<Vec<Finding>> {
    let json: BuildifierOutput = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "failed to parse buildifier format JSON output; raw stdout: {:?}",
            String::from_utf8_lossy(stdout)
        )
    })?;

    let mut findings = Vec::new();
    for file in json.files {
        if !file.formatted.unwrap_or(true) {
            findings.push(Finding {
                severity: Severity::Warning,
                message: "file needs buildifier formatting".to_owned(),
                location: Some(Location {
                    path: file_path.to_path_buf(),
                    line: None,
                    column: None,
                }),
                remediations: vec![format!(
                    "Run `buildifier {}` to auto-format.",
                    file_path.display()
                )],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

/// Parses `--mode=check --lint=warn --format=json` output and returns one finding per warning.
pub(crate) fn parse_lint_output(stdout: &[u8], file_path: &Path) -> Result<Vec<Finding>> {
    let json: BuildifierOutput = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "failed to parse buildifier lint JSON output; raw stdout: {:?}",
            String::from_utf8_lossy(stdout)
        )
    })?;

    let mut findings = Vec::new();
    for file in json.files {
        for warning in file.warnings.unwrap_or_default() {
            findings.push(Finding {
                severity: Severity::Warning,
                message: format!("{}: {}", warning.category, warning.message),
                location: Some(Location {
                    path: file_path.to_path_buf(),
                    line: Some(warning.start.line),
                    column: Some(warning.start.column),
                }),
                remediations: vec![
                    format!(
                        "Run `buildifier --lint=fix {}` to auto-fix, or resolve manually.",
                        file_path.display()
                    ),
                ],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

// ── JSON types ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BuildifierOutput {
    #[serde(default)]
    files: Vec<BuildifierFile>,
}

#[derive(Debug, Deserialize)]
struct BuildifierFile {
    formatted: Option<bool>,
    #[serde(default)]
    warnings: Option<Vec<BuildifierWarning>>,
}

#[derive(Debug, Deserialize)]
struct BuildifierWarning {
    start: BuildifierPosition,
    category: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct BuildifierPosition {
    line: u32,
    column: u32,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;
    use toml::toml;

    use super::{BuildifierCheck, is_buildifier_file, parse_format_output, parse_lint_output};
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    // ── config parsing tests ─────────────────────────────────────────────────

    #[test]
    fn config_defaults_to_buildifier_target() {
        let check = BuildifierCheck;
        let config = check.configure(&toml::Value::Table(Default::default())).unwrap();
        // Verify it configures without error — the default is @buildifier_prebuilt//:buildifier.
        drop(config);
    }

    #[test]
    fn config_explicit_buildifier_path_accepted() {
        let check = BuildifierCheck;
        check
            .configure(&toml::Value::Table(toml! {
                buildifier_path = "bin/buildifier"
            }))
            .expect("explicit buildifier_path should be accepted");
    }

    #[test]
    fn config_explicit_buildifier_target_accepted() {
        let check = BuildifierCheck;
        check
            .configure(&toml::Value::Table(toml! {
                buildifier_target = "@buildifier_prebuilt//:buildifier"
            }))
            .expect("explicit buildifier_target should be accepted");
    }

    #[test]
    fn config_both_keys_rejected() {
        let check = BuildifierCheck;
        let result = check.configure(&toml::Value::Table(toml! {
            buildifier_path = "bin/buildifier"
            buildifier_target = "@buildifier_prebuilt//:buildifier"
        }));
        assert!(result.is_err(), "expected an error when both keys are set");
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("mutually exclusive"),
            "expected mutually-exclusive error, got: {msg}"
        );
    }

    // ── format JSON parser tests ─────────────────────────────────────────────

    #[test]
    fn format_output_detects_unformatted_file() {
        let json = br#"{"success":false,"files":[{"filename":"foo.bzl","formatted":false}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("formatting"), "unexpected: {}", findings[0].message);
        assert!(findings[0].location.as_ref().unwrap().line.is_none());
    }

    #[test]
    fn format_output_no_finding_when_formatted() {
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl","formatted":true}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn format_output_no_finding_when_formatted_absent() {
        // `formatted` absent → treated as true (already formatted)
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl"}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    // ── lint JSON parser tests ───────────────────────────────────────────────

    #[test]
    fn lint_output_parses_single_warning() {
        let json = br#"{
            "success": false,
            "files": [{
                "filename": "foo.bzl",
                "warnings": [{
                    "filename": "foo.bzl",
                    "start": {"line": 10, "column": 5},
                    "end": {"line": 10, "column": 5},
                    "category": "module-docstring",
                    "actionable": true,
                    "message": "The file has no module docstring."
                }]
            }]
        }"#;
        let findings = parse_lint_output(json, Path::new("foo.bzl")).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("module-docstring"), "unexpected: {}", f.message);
        let loc = f.location.as_ref().unwrap();
        assert_eq!(loc.line, Some(10));
        assert_eq!(loc.column, Some(5));
    }

    #[test]
    fn lint_output_parses_multiple_warnings() {
        let json = br#"{
            "success": false,
            "files": [{
                "filename": "BUILD",
                "warnings": [
                    {"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 1},
                     "category": "module-docstring", "actionable": true,
                     "message": "missing docstring"},
                    {"start": {"line": 5, "column": 3}, "end": {"line": 5, "column": 3},
                     "category": "no-effect", "actionable": true,
                     "message": "expression has no effect"}
                ]
            }]
        }"#;
        let findings = parse_lint_output(json, Path::new("BUILD")).unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].location.as_ref().unwrap().line, Some(1));
        assert_eq!(findings[1].location.as_ref().unwrap().line, Some(5));
    }

    #[test]
    fn lint_output_no_findings_when_warnings_absent() {
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl"}]}"#;
        let findings = parse_lint_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn lint_output_no_findings_when_warnings_empty() {
        // Regression: combined --mode=check --lint=warn output for a clean file has
        // `"warnings":[]` rather than the field being absent.
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl","formatted":true,"valid":true,"warnings":[]}]}"#;
        let findings = parse_lint_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn lint_output_combined_mode_check_shape_with_warning() {
        // Regression: buildifier --mode=check --lint=warn --format=json output for
        // lib/rust/broker-robinhood/BUILD.bazel — the "load" warning was parsed incorrectly
        // before the lint invocation was fixed to include --mode=check.
        let json = br#"{"success":false,"files":[{"filename":"lib/rust/broker-robinhood/BUILD.bazel","formatted":false,"valid":true,"warnings":[{"start":{"line":2,"column":37},"end":{"line":2,"column":48},"category":"load","actionable":true,"autoFixable":true,"message":"Loaded symbol \"rust_binary\" is unused. Please remove it.\nTo disable the warning, add '@unused' in a comment.","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#load"}]}]}"#;
        let findings =
            parse_lint_output(json, Path::new("lib/rust/broker-robinhood/BUILD.bazel")).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert!(f.message.contains("load"), "unexpected: {}", f.message);
        assert_eq!(f.location.as_ref().unwrap().line, Some(2));
        assert_eq!(f.location.as_ref().unwrap().column, Some(37));
    }

    #[test]
    fn lint_parse_error_includes_raw_stdout() {
        // When JSON parsing fails, the error message must include the raw stdout so the
        // failure is diagnosable without re-running buildifier.
        let garbage = b"buildifier: cannot specify --format without --mode=check\n";
        let err = parse_lint_output(garbage, Path::new("foo.bzl")).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("cannot specify --format"),
            "raw stdout must appear in error: {msg}"
        );
    }

    // ── file-kind filter ─────────────────────────────────────────────────────

    #[test]
    fn recognises_bzl_and_star_extensions() {
        assert!(is_buildifier_file(Path::new("rules.bzl")));
        assert!(is_buildifier_file(Path::new("lib/helpers.bzl")));
        assert!(is_buildifier_file(Path::new("macros.star")));
    }

    #[test]
    fn recognises_special_filenames() {
        for name in [
            "BUILD",
            "BUILD.bazel",
            "MODULE.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
        ] {
            assert!(
                is_buildifier_file(Path::new(name)),
                "{name} should be recognised as a Starlark file"
            );
        }
    }

    #[test]
    fn rejects_non_starlark_files() {
        for name in ["main.rs", "Cargo.toml", "README.md", "script.py", "foo.txt"] {
            assert!(
                !is_buildifier_file(Path::new(name)),
                "{name} should not be recognised as a Starlark file"
            );
        }
    }

    // ── integration: changeset scoping (no buildifier binary required) ───────

    #[tokio::test]
    async fn non_starlark_file_in_changeset_produces_no_findings() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("main.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .unwrap();

        assert!(
            result.findings.is_empty(),
            "non-Starlark files must be skipped; got: {:?}",
            result.findings
        );
    }

    #[tokio::test]
    async fn deleted_starlark_file_produces_no_findings() {
        let temp = tempdir().unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("deleted.bzl").to_path_buf(),
                    kind: ChangeKind::Deleted,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .unwrap();

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn both_checks_disabled_produces_no_findings() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("file.bzl"), "def foo(): pass\n").unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("file.bzl").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml! {
                    check_format = false
                    check_lint = false
                }),
            )
            .await
            .unwrap();

        assert!(result.findings.is_empty());
    }
}
