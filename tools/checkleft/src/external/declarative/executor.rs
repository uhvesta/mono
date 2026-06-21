//! Invocation orchestration for declarative checks.
//!
//! Pipeline: select matched files → resolve declared binaries → run each
//! invocation (batch or per-file) → apply exit semantics → project stdout into
//! findings → concatenate. The framework owns every step; the check is data.
//!
//! Exit semantics are load-bearing. A nonzero/`default → error` outcome aborts the
//! whole check with an error (surfaced by the runner as a check error), so a tool
//! that crashes never masquerades as "clean". `findings` runs the transform (which
//! naturally yields zero findings for clean output); `ok` short-circuits to none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};

use crate::input::{ChangeKind, ChangeSet};
use crate::output::{CheckResult, Finding, Severity};

use super::{
    ArtifactFormat, BazelAspectInvocation, ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind,
    InvocationMode, ToolInvocation, resolve,
};

/// Run a declarative check end-to-end. `repo_root` is the working directory
/// invocations run from (and the Bazel workspace, when the `bazel` resolver is used).
///
/// `effective_severity` is the policy severity configured for this check instance
/// (from CHECKS.yaml). Declarative invocations may use it — e.g. `bazel_aspect`
/// supports `{{severity_deny_flag}}` in `build_flags`, which expands to `-Dwarnings`
/// when the severity is `error` and is dropped otherwise.
pub fn run_declarative_check(
    repo_root: &Path,
    package_id: &str,
    package: &ExternalCheckDeclarativePackage,
    changeset: &ChangeSet,
    config: &toml::Value,
    effective_severity: Option<Severity>,
) -> Result<CheckResult> {
    // A per-repo `applies_to` override in the CHECKS.yaml config blob replaces the
    // definition's applies_to list entirely (same glob vocabulary, replace semantics).
    let applies_to_override = resolve::override_applies_to(config)
        .transpose()
        .context("invalid `applies_to` config override")?;
    let applies_to: &[String] = applies_to_override.as_deref().unwrap_or(&package.applies_to);
    let files = select_files(changeset, applies_to)?;
    if files.is_empty() {
        return Ok(CheckResult {
            check_id: package_id.to_owned(),
            findings: Vec::new(),
        });
    }

    // Resolution is only needed for tool invocations; bazel_aspect invocations
    // delegate to bazel and declare no binaries (needs may be empty).
    let binaries = if package
        .invocations
        .iter()
        .any(|invocation| matches!(invocation.kind, InvocationKind::Tool(_)))
    {
        resolve::resolve_all(repo_root, &package.needs, config)?
    } else {
        BTreeMap::new()
    };

    let mut findings = Vec::new();
    for invocation in &package.invocations {
        findings.extend(run_invocation(
            repo_root,
            &binaries,
            invocation,
            &files,
            effective_severity,
        )?);
    }

    // Deduplicate: some tools (e.g. rustfmt with per-file + module-tree recursion)
    // report the same file via multiple code paths. Keep first occurrence of each
    // (check_id, path, line, column, message, severity) tuple.
    findings = dedup_findings(findings);

    Ok(CheckResult {
        check_id: package_id.to_owned(),
        findings,
    })
}

/// Remove findings that describe the same location + issue as an earlier finding.
/// Key is `(path, line, column, message, severity)` — remediations are intentionally
/// excluded: two invocations may reach the same file via different code paths (e.g.
/// rustfmt recursing from a module root and directly from the child file), producing
/// the same issue but with slightly different remediation text.
fn dedup_findings(findings: Vec<Finding>) -> Vec<Finding> {
    use std::collections::HashSet;
    type Key = (Option<PathBuf>, Option<u32>, Option<u32>, String, u8);
    let mut seen: HashSet<Key> = HashSet::new();
    let mut out: Vec<Finding> = Vec::with_capacity(findings.len());
    for f in findings {
        let key: Key = (
            f.location.as_ref().map(|l| l.path.clone()),
            f.location.as_ref().and_then(|l| l.line),
            f.location.as_ref().and_then(|l| l.column),
            f.message.clone(),
            f.severity as u8,
        );
        if seen.insert(key) {
            out.push(f);
        }
    }
    out
}

/// Strip `repo_root` from any absolute finding paths so every finding emitted by
/// the declarative runtime is repo-relative. This is a framework-level guarantee:
/// tools may receive absolute paths (e.g. when the hermetic Bazel toolchain wrapper
/// canonicalizes inputs) and echo them back — the framework normalises before the
/// finding reaches the runner.
fn normalize_finding_paths(findings: &mut [Finding], repo_root: &Path) {
    for finding in findings.iter_mut() {
        if let Some(location) = &mut finding.location
            && location.path.is_absolute()
            && let Ok(relative) = location.path.strip_prefix(repo_root)
        {
            location.path = relative.to_path_buf();
        }
    }
}

/// Select non-deleted changed files matching any `applies_to` glob, sorted for
/// determinism.
fn select_files(changeset: &ChangeSet, applies_to: &[String]) -> Result<Vec<String>> {
    let mut builder = GlobSetBuilder::new();
    for pattern in applies_to {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid applies_to glob `{pattern}`"))?);
    }
    let globset = builder.build().context("failed to build applies_to glob set")?;

    let mut files: Vec<String> = changeset
        .changed_files
        .iter()
        .filter(|file| !matches!(file.kind, ChangeKind::Deleted))
        .filter(|file| globset.is_match(&file.path))
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn run_invocation(
    repo_root: &Path,
    binaries: &BTreeMap<String, PathBuf>,
    invocation: &Invocation,
    files: &[String],
    effective_severity: Option<Severity>,
) -> Result<Vec<Finding>> {
    let mut findings = match &invocation.kind {
        InvocationKind::Tool(tool) => run_tool_invocation(repo_root, binaries, invocation, tool, files)?,
        InvocationKind::BazelAspect(aspect) => {
            run_bazel_aspect_invocation(repo_root, invocation, aspect, files, effective_severity)?
        }
    };

    // Normalize absolute paths to repo-relative. Tools invoked via the hermetic
    // Bazel toolchain wrapper may receive absolute input paths and echo them back;
    // the framework strips the repo root prefix before the finding reaches the runner.
    normalize_finding_paths(&mut findings, repo_root);
    Ok(findings)
}

fn run_tool_invocation(
    repo_root: &Path,
    binaries: &BTreeMap<String, PathBuf>,
    invocation: &Invocation,
    tool: &ToolInvocation,
    files: &[String],
) -> Result<Vec<Finding>> {
    let binary = binaries
        .get(&tool.run)
        .ok_or_else(|| anyhow::anyhow!("invocation `{}` binary `{}` was not resolved", invocation.id, tool.run))?;

    match tool.mode {
        InvocationMode::Batch => {
            let args = expand_batch_args(repo_root, &tool.args, files);
            let output = spawn(repo_root, binary, &args, &invocation.id)?;
            classify_and_project(invocation, &output, None)
        }
        InvocationMode::PerFile => {
            let mut findings = Vec::new();
            for file in files {
                let args = expand_per_file_args(repo_root, &tool.args, file);
                let output = spawn(repo_root, binary, &args, &invocation.id)?;
                findings.extend(classify_and_project(invocation, &output, Some(file))?);
            }
            Ok(findings)
        }
    }
}

/// Expand `{{severity_deny_flag}}` in a single `build_flags` entry.
///
/// Returns `Some(expanded)` to keep the entry (with the template replaced), or
/// `None` to drop the entry entirely. Currently the only recognised template is
/// `{{severity_deny_flag}}`, which resolves to `-Dwarnings` when the effective
/// severity is `Error` (so clippy violations fail the build action) and causes
/// the containing flag to be dropped when severity is anything else.
fn expand_build_flag(flag: &str, effective_severity: Option<Severity>) -> Option<String> {
    const SEVERITY_DENY_FLAG: &str = "{{severity_deny_flag}}";
    if flag.contains(SEVERITY_DENY_FLAG) {
        match effective_severity {
            Some(Severity::Error) => Some(flag.replace(SEVERITY_DENY_FLAG, "-Dwarnings")),
            _ => None,
        }
    } else {
        Some(flag.to_owned())
    }
}

/// Run a bazel_aspect invocation: map the matched files to their owning targets,
/// build those targets with the declared aspect, then read the output-group
/// artifacts and project each through the transform.
///
/// The build is fully cached by bazel: when CI (or the developer) has already run
/// a build with the same aspect and flags, every action is a cache hit and this
/// reduces to artifact lookup. Freshness is bazel's guarantee — checkleft never
/// reads an artifact bazel did not just account for.
fn run_bazel_aspect_invocation(
    repo_root: &Path,
    invocation: &Invocation,
    aspect: &BazelAspectInvocation,
    files: &[String],
    effective_severity: Option<Severity>,
) -> Result<Vec<Finding>> {
    let bazel = PathBuf::from("bazel");

    // 1. Map changed files to the targets that own them. `same_pkg_direct_rdeps`
    //    resolves each source file to the rule(s) listing it in srcs. Files that
    //    bazel does not know (not in any package) would fail the query; tolerate
    //    partial results via --keep_going (exit 3 = partial success).
    let set = files
        .iter()
        .map(|file| format!("'{}'", file.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!("same_pkg_direct_rdeps(set({set}))");
    let query_args = vec![
        "query".to_owned(),
        "--keep_going".to_owned(),
        "--output=label".to_owned(),
        query,
    ];
    let query_output = spawn(repo_root, &bazel, &query_args, &invocation.id)?;
    if !matches!(query_output.exit_code, Some(0) | Some(3)) {
        bail!(
            "bazel_aspect invocation `{}`: target query failed (exit {}): {}",
            invocation.id,
            describe_exit(query_output.exit_code),
            String::from_utf8_lossy(&query_output.stderr).trim()
        );
    }
    let mut targets: Vec<String> = String::from_utf8_lossy(&query_output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("//") || line.starts_with('@'))
        .map(str::to_owned)
        .collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Build the targets with the aspect. Exit semantics from the manifest
    //    classify the build's exit code (typically `0 → findings, default → error`).
    let aspect_flags = [
        format!("--aspects={}", aspect.aspect),
        format!("--output_groups={}", aspect.output_groups.join(",")),
    ];
    let expanded_build_flags: Vec<String> = aspect
        .build_flags
        .iter()
        .filter_map(|f| expand_build_flag(f, effective_severity))
        .collect();
    let mut build_args: Vec<String> = vec!["build".to_owned()];
    build_args.extend(aspect_flags.iter().cloned());
    build_args.extend(expanded_build_flags.iter().cloned());
    build_args.extend(targets.iter().cloned());
    let build_output = spawn(repo_root, &bazel, &build_args, &invocation.id)?;
    match invocation.exit.classify(build_output.exit_code) {
        ExitOutcome::Ok => return Ok(Vec::new()),
        ExitOutcome::Findings => {}
        ExitOutcome::Error => bail!(
            "bazel_aspect invocation `{}`: bazel build exited with status {} (treated as error by exit semantics): {}",
            invocation.id,
            describe_exit(build_output.exit_code),
            String::from_utf8_lossy(&build_output.stderr).trim()
        ),
    }

    // 3. Discover the artifact files the output groups produced. cquery with the
    //    same aspect/flags prints one workspace-relative path per line. Unlike
    //    `build`, cquery takes a single query EXPRESSION — multiple positional
    //    targets are a parse error — so the targets are wrapped in `set(...)`.
    let mut cquery_args: Vec<String> = vec!["cquery".to_owned()];
    cquery_args.extend(aspect_flags.iter().cloned());
    cquery_args.extend(expanded_build_flags.iter().cloned());
    cquery_args.push("--output=files".to_owned());
    cquery_args.push(format!("set({})", targets.join(" ")));
    let cquery_output = spawn(repo_root, &bazel, &cquery_args, &invocation.id)?;
    if cquery_output.exit_code != Some(0) {
        bail!(
            "bazel_aspect invocation `{}`: artifact discovery (cquery --output=files) failed (exit {}): {}",
            invocation.id,
            describe_exit(cquery_output.exit_code),
            String::from_utf8_lossy(&cquery_output.stderr).trim()
        );
    }

    // 4. Read each artifact and project it through the transform. Empty artifacts
    //    are clean results (e.g. a crate with no clippy diagnostics).
    let mut findings = Vec::new();
    for artifact in String::from_utf8_lossy(&cquery_output.stdout).lines() {
        let artifact = artifact.trim();
        if artifact.is_empty() {
            continue;
        }
        let path = repo_root.join(artifact);
        let contents = std::fs::read(&path).with_context(|| {
            format!(
                "bazel_aspect invocation `{}`: failed to read artifact `{}`",
                invocation.id,
                path.display()
            )
        })?;
        if contents.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let document = match aspect.artifact_format {
            ArtifactFormat::Json => contents,
            ArtifactFormat::JsonLines => jsonl_to_array(&contents).with_context(|| {
                format!(
                    "bazel_aspect invocation `{}`: artifact `{}` is not valid JSONL",
                    invocation.id,
                    path.display()
                )
            })?,
        };
        findings.extend(invocation.transform.apply(&document, Some(0), None).with_context(|| {
            format!(
                "transform for invocation `{}` failed on artifact `{}`",
                invocation.id, artifact
            )
        })?);
    }
    Ok(findings)
}

/// Normalise a JSONL document (one JSON value per non-empty line) into a single
/// JSON array, so `json` transforms can select over it with `.[] | ...`.
pub(super) fn jsonl_to_array(contents: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(contents).context("artifact is not valid UTF-8")?;
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("line {} is not valid JSON: {line:?}", index + 1))?;
        values.push(value);
    }
    serde_json::to_vec(&serde_json::Value::Array(values)).context("failed to serialise JSONL array")
}

fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(code) => code.to_string(),
        None => "signal".to_owned(),
    }
}

/// In batch mode the standalone `{{files}}` arg expands to N file args.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
fn expand_batch_args(repo_root: &Path, args: &[String], files: &[String]) -> Vec<String> {
    let repo_root_str = repo_root.to_string_lossy();
    let mut expanded = Vec::with_capacity(args.len() + files.len());
    for arg in args {
        if arg == "{{files}}" {
            expanded.extend(files.iter().cloned());
        } else {
            expanded.push(arg.replace("{{repo_root}}", &repo_root_str));
        }
    }
    expanded
}

/// In per-file mode `{{file}}` is substituted in place within each arg.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
fn expand_per_file_args(repo_root: &Path, args: &[String], file: &str) -> Vec<String> {
    let repo_root_str = repo_root.to_string_lossy();
    args.iter()
        .map(|arg| arg.replace("{{file}}", file).replace("{{repo_root}}", &repo_root_str))
        .collect()
}

struct InvocationOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

fn spawn(repo_root: &Path, binary: &Path, args: &[String], invocation_id: &str) -> Result<InvocationOutput> {
    let output = Command::new(binary)
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| {
            format!(
                "failed to spawn declarative invocation `{invocation_id}` binary `{}`",
                binary.display()
            )
        })?;
    Ok(InvocationOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

fn classify_and_project(
    invocation: &Invocation,
    output: &InvocationOutput,
    input_file: Option<&str>,
) -> Result<Vec<Finding>> {
    match invocation.exit.classify(output.exit_code) {
        ExitOutcome::Ok => Ok(Vec::new()),
        ExitOutcome::Findings => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            invocation
                .transform
                .apply(&output.stdout, output.exit_code, input_file)
                .with_context(|| {
                    let file_note = input_file.map(|f| format!(" (file: {f})")).unwrap_or_default();
                    let stderr_trimmed = stderr.trim();
                    if stderr_trimmed.is_empty() {
                        format!("transform for invocation `{}`{file_note} failed", invocation.id)
                    } else {
                        format!(
                            "transform for invocation `{}`{file_note} failed; tool stderr:\n{stderr_trimmed}",
                            invocation.id
                        )
                    }
                })
        }
        ExitOutcome::Error => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "declarative invocation `{}` exited with status {} (treated as error by exit semantics): {}",
                invocation.id,
                output
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned()),
                stderr.trim()
            )
        }
    }
}
