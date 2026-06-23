//! Invocation orchestration for declarative checks.
//!
//! Pipeline: select matched files → resolve declared binaries → run each
//! invocation (batch or per-file) → apply exit semantics → project stdout into
//! findings → concatenate. The framework owns every step; the check is data.
//!
//! Exit semantics are load-bearing and differ by invocation mode:
//!
//! - **Batch** (single invocation over all files): a `default → error` outcome aborts
//!   the whole check. The invocation has no per-file scope, so there is nowhere to
//!   attach a file-scoped error finding — the whole check is the unit of failure.
//! - **Per-file** (one invocation per file): a `default → error` outcome for one file
//!   is **isolated** — it becomes an error-severity finding scoped to that file, and
//!   the loop continues to the next file. This prevents a single bad file (e.g. a
//!   symlink that the tool refuses) from masking every other file's findings.
//!
//! In both modes, an `ok` exit short-circuits to no findings; a `findings` exit runs
//! the transform (which naturally yields zero findings for clean output). The
//! "never masquerade as clean" invariant is preserved in per_file mode: an errored
//! file surfaces as an error-severity finding, so the check still fails — it just
//! no longer suppresses the other files' results.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};

use crate::external::sandbox::HostCeiling;
use crate::fix::safety::WritableSandbox;
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

use super::{
    ArtifactFormat, BazelAspectInvocation, ExitOutcome, ExternalCheckDeclarativePackage, FixBlock, FixExitOutcome,
    Invocation, InvocationKind, InvocationMode, ToolInvocation, resolve,
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
    run_declarative_check_impl(
        repo_root,
        package_id,
        package,
        changeset,
        config,
        effective_severity,
        None,
    )
}

/// Like [`run_declarative_check`] but emits per-file/per-chunk progress ticks
/// via `on_file_processed` (cumulative count of eligible files processed so far).
pub(crate) fn run_declarative_check_with_progress(
    repo_root: &Path,
    package_id: &str,
    package: &ExternalCheckDeclarativePackage,
    changeset: &ChangeSet,
    config: &toml::Value,
    effective_severity: Option<Severity>,
    on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
) -> Result<CheckResult> {
    run_declarative_check_impl(
        repo_root,
        package_id,
        package,
        changeset,
        config,
        effective_severity,
        Some(on_file_processed),
    )
}

fn run_declarative_check_impl(
    repo_root: &Path,
    package_id: &str,
    package: &ExternalCheckDeclarativePackage,
    changeset: &ChangeSet,
    config: &toml::Value,
    effective_severity: Option<Severity>,
    on_file_processed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
) -> Result<CheckResult> {
    // A per-repo `applies_to` override in the CHECKS.yaml config blob replaces the
    // definition's applies_to list entirely (same glob vocabulary, replace semantics).
    let applies_to_override = resolve::override_applies_to(config)
        .transpose()
        .context("invalid `applies_to` config override")?;
    let applies_to: &[String] = applies_to_override.as_deref().unwrap_or(&package.applies_to);
    let files = select_files(repo_root, changeset, applies_to, package.skip_symlinks)?;
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

    // Extract top-level string values from the config blob for {{config.KEY}}
    // expansion in invocation args. Framework-level keys (needs, applies_to) are
    // consumed by the binary resolver and apply-to override; remaining string
    // values are user-defined parameters (e.g. config_file for lint/js).
    let config_values = extract_config_string_values(config);

    let mut findings = Vec::new();
    for invocation in &package.invocations {
        findings.extend(run_invocation(
            repo_root,
            &binaries,
            invocation,
            &files,
            effective_severity,
            &config_values,
            on_file_processed.as_deref(),
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

/// Count the files in `changeset` that the declarative check will actually process
/// after applying the `applies_to` glob filter and any per-repo config override.
/// Falls back to the full changeset size on glob-build errors.
///
/// Mirrors the first step of [`run_declarative_check`] so the runner can seed the
/// progress reporter with the correct per-check eligible count before execution.
pub(crate) fn eligible_file_count(
    repo_root: &Path,
    package: &ExternalCheckDeclarativePackage,
    changeset: &ChangeSet,
    config: &toml::Value,
) -> usize {
    // A malformed override causes run_declarative_check to fail; here we fall back to
    // the full changeset size so the progress count is conservative and consistent with
    // the "something went wrong" signal the runner will surface when the check runs.
    let applies_to_override = match resolve::override_applies_to(config) {
        Some(Ok(globs)) => Some(globs),
        Some(Err(_)) => return changeset.changed_files.len(),
        None => None,
    };
    let applies_to: &[String] = applies_to_override.as_deref().unwrap_or(&package.applies_to);
    select_files(repo_root, changeset, applies_to, package.skip_symlinks)
        .map(|f| f.len())
        .unwrap_or_else(|_| changeset.changed_files.len())
}

/// Select non-deleted changed files matching any `applies_to` glob, sorted for
/// determinism. When `skip_symlinks` is true, paths that are symlinks (resolved
/// against `repo_root`) are excluded without following the link.
fn select_files(
    repo_root: &Path,
    changeset: &ChangeSet,
    applies_to: &[String],
    skip_symlinks: bool,
) -> Result<Vec<String>> {
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
        .filter(|file| {
            if !skip_symlinks {
                return true;
            }
            let abs = repo_root.join(&file.path);
            !std::fs::symlink_metadata(&abs)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        })
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn run_invocation(
    repo_root: &Path,
    binaries: &BTreeMap<String, resolve::ResolvedBinary>,
    invocation: &Invocation,
    files: &[String],
    effective_severity: Option<Severity>,
    config_values: &BTreeMap<String, String>,
    on_file_processed: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> Result<Vec<Finding>> {
    let mut findings = match &invocation.kind {
        InvocationKind::Tool(tool) => run_tool_invocation(
            repo_root,
            binaries,
            invocation,
            tool,
            files,
            config_values,
            on_file_processed,
        )?,
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
    binaries: &BTreeMap<String, resolve::ResolvedBinary>,
    invocation: &Invocation,
    tool: &ToolInvocation,
    files: &[String],
    config_values: &BTreeMap<String, String>,
    on_file_processed: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> Result<Vec<Finding>> {
    let binary = binaries
        .get(&tool.run)
        .ok_or_else(|| anyhow::anyhow!("invocation `{}` binary `{}` was not resolved", invocation.id, tool.run))?;

    // Resolved prefix args (e.g. `npx --yes eslint@10.5.0`) precede the
    // invocation's own templated args.
    let with_prefix = |args: Vec<String>| -> Vec<String> {
        let mut all = binary.prefix_args.clone();
        all.extend(args);
        all
    };

    // Build the human-readable invocation map for {{needs.<name>.invocation}} templates.
    let needs_invocations: BTreeMap<String, String> = binaries
        .iter()
        .map(|(name, bin)| (name.clone(), bin.display_invocation.clone()))
        .collect();

    match tool.mode {
        InvocationMode::Batch => {
            // Compute the fixed argv cost (program + prefix args + non-{{files}} args)
            // to size file chunks. Files are split into chunks that keep the total
            // argv byte cost under ARG_BYTE_SAFE_THRESHOLD to avoid ARG_MAX errors
            // on large changesets.
            let fixed_cost = argv_byte_cost_of_path(&binary.program)
                + argv_byte_cost(&binary.prefix_args)
                + tool
                    .args
                    .iter()
                    .filter(|a| *a != "{{files}}")
                    .map(|a| a.replace("{{repo_root}}", &repo_root.to_string_lossy()))
                    .map(|a| a.len() + 1)
                    .sum::<usize>();
            let chunks = split_files_into_chunks(fixed_cost, files);
            let mut findings = Vec::new();
            // Cumulative file count for progress reporting. Each chunk is reported
            // as a single tick (one batch invocation per chunk).
            let mut processed = 0usize;
            for chunk in chunks {
                let args = with_prefix(expand_batch_args(repo_root, &tool.args, chunk, config_values)?);
                let output = spawn(repo_root, &binary.program, &args, &invocation.id)?;
                findings.extend(classify_and_project(
                    invocation,
                    &output,
                    None,
                    Some(&needs_invocations),
                )?);
                processed += chunk.len();
                if let Some(cb) = on_file_processed {
                    cb(processed);
                }
            }
            Ok(findings)
        }
        InvocationMode::PerFile => {
            // Per-file errors are isolated: one file's error becomes an error-severity
            // finding scoped to that file, and the loop continues to the next file.
            // See module-level doc for the batch vs per_file asymmetry.
            let mut findings = Vec::new();
            let mut processed = 0usize;
            for file in files {
                let args = with_prefix(expand_per_file_args(repo_root, &tool.args, file, config_values)?);
                let output = spawn(repo_root, &binary.program, &args, &invocation.id)?;
                match invocation.exit.classify(output.exit_code) {
                    ExitOutcome::Ok => {}
                    ExitOutcome::Findings => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        findings.extend(
                            invocation
                                .transform
                                .apply(&output.stdout, output.exit_code, Some(file), Some(&needs_invocations))
                                .with_context(|| {
                                    let stderr_trimmed = stderr.trim();
                                    if stderr_trimmed.is_empty() {
                                        format!("transform for invocation `{}` (file: {file}) failed", invocation.id)
                                    } else {
                                        format!(
                                            "transform for invocation `{}` (file: {file}) failed; tool stderr:\n{stderr_trimmed}",
                                            invocation.id
                                        )
                                    }
                                })?,
                        );
                    }
                    ExitOutcome::Error => {
                        // Record an error finding for this file and continue.
                        // The check still fails (error-severity finding), but other
                        // files' findings are not suppressed.
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        findings.push(Finding {
                            severity: Severity::Error,
                            message: format!(
                                "declarative invocation `{}` failed for `{}` (exit {}): {}",
                                invocation.id,
                                file,
                                describe_exit(output.exit_code),
                                stderr.trim()
                            ),
                            location: Some(Location {
                                path: PathBuf::from(file),
                                line: None,
                                column: None,
                            }),
                            remediations: Vec::new(),
                            suggested_fix: None,
                        });
                    }
                }
                processed += 1;
                if let Some(cb) = on_file_processed {
                    cb(processed);
                }
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
    //    are clean results (e.g. a crate with no clippy diagnostics). bazel_aspect
    //    invocations delegate to bazel and declare no binaries, so there are no
    //    needs_invocations to thread through here.
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
        findings.extend(
            invocation
                .transform
                .apply(&document, Some(0), None, None)
                .with_context(|| {
                    format!(
                        "transform for invocation `{}` failed on artifact `{}`",
                        invocation.id, artifact
                    )
                })?,
        );
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

/// Safe upper bound on argv byte cost per invocation, well below the OS ARG_MAX.
/// Linux's ARG_MAX is 2 MiB; macOS allows ~1 MiB but shrinks with large environments.
/// 128 KiB gives generous headroom on every platform.
pub(crate) const ARG_BYTE_SAFE_THRESHOLD: usize = 128 * 1024;

/// Approximate argv byte cost of a slice of argument strings.
/// Each argument contributes its byte length plus one byte for its null terminator.
fn argv_byte_cost(args: &[String]) -> usize {
    args.iter().map(|a| a.len() + 1).sum()
}

/// Approximate argv byte cost of a path (used for the program argument).
fn argv_byte_cost_of_path(path: &Path) -> usize {
    path.to_string_lossy().len() + 1
}

/// Split `files` into the smallest number of contiguous slices such that each
/// slice, when added to `fixed_cost`, keeps the total argv byte cost under
/// [`ARG_BYTE_SAFE_THRESHOLD`]. When a single file alone would exceed the
/// threshold there is no smaller unit to split at, so it is placed in its own
/// chunk and the invocation proceeds (the OS may still succeed).
pub(crate) fn split_files_into_chunks(fixed_cost: usize, files: &[String]) -> Vec<&[String]> {
    if files.is_empty() {
        return vec![files];
    }
    let available = ARG_BYTE_SAFE_THRESHOLD.saturating_sub(fixed_cost);
    let mut chunks: Vec<&[String]> = Vec::new();
    let mut start = 0;
    let mut chunk_cost: usize = 0;

    for (i, file) in files.iter().enumerate() {
        let file_cost = file.len() + 1;
        if chunk_cost + file_cost > available && i > start {
            chunks.push(&files[start..i]);
            start = i;
            chunk_cost = 0;
        }
        chunk_cost += file_cost;
    }
    chunks.push(&files[start..]);
    chunks
}

/// In batch mode the standalone `{{files}}` arg expands to N file args.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
/// `{{config.KEY}}` is substituted with the value of KEY from the config blob.
fn expand_batch_args(
    repo_root: &Path,
    args: &[String],
    files: &[String],
    config_values: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let repo_root_str = repo_root.to_string_lossy();
    let mut expanded = Vec::with_capacity(args.len() + files.len());
    for arg in args {
        if arg == "{{files}}" {
            expanded.extend(files.iter().cloned());
        } else {
            expanded.push(expand_config_refs(
                &arg.replace("{{repo_root}}", &repo_root_str),
                config_values,
            )?);
        }
    }
    Ok(expanded)
}

/// In per-file mode `{{file}}` is substituted in place within each arg.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
/// `{{config.KEY}}` is substituted with the value of KEY from the config blob.
fn expand_per_file_args(
    repo_root: &Path,
    args: &[String],
    file: &str,
    config_values: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let repo_root_str = repo_root.to_string_lossy();
    args.iter()
        .map(|arg| {
            expand_config_refs(
                &arg.replace("{{file}}", file).replace("{{repo_root}}", &repo_root_str),
                config_values,
            )
        })
        .collect()
}

/// Substitute `{{config.KEY}}` refs in a single arg string. Each ref is replaced
/// with the corresponding string value from `config_values`. Missing keys produce
/// a clear error directing the operator to add the key to their CHECKS config.
fn expand_config_refs(arg: &str, config_values: &BTreeMap<String, String>) -> Result<String> {
    const CONFIG_PREFIX: &str = "{{config.";
    const CLOSE: &str = "}}";
    if !arg.contains(CONFIG_PREFIX) {
        return Ok(arg.to_owned());
    }
    let mut result = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(open) = rest.find(CONFIG_PREFIX) {
        result.push_str(&rest[..open]);
        let after = &rest[open + CONFIG_PREFIX.len()..];
        let close = after
            .find(CLOSE)
            .ok_or_else(|| anyhow::anyhow!("unterminated `{{{{config.` in arg `{arg}`"))?;
        let key = &after[..close];
        let value = config_values.get(key).ok_or_else(|| {
            anyhow::anyhow!(
                "required config key `{key}` is not set — \
                 add `{key}: <value>` to this check's `config:` block in CHECKS.yaml"
            )
        })?;
        result.push_str(value);
        rest = &after[close + CLOSE.len()..];
    }
    result.push_str(rest);
    Ok(result)
}

/// Extract top-level string values from a TOML config blob for `{{config.KEY}}`
/// arg expansion. Only immediate string leaves at the table root are included;
/// nested tables (e.g. `needs.*`, `applies_to`) are framework-level and skipped.
fn extract_config_string_values(config: &toml::Value) -> BTreeMap<String, String> {
    let Some(table) = config.as_table() else {
        return BTreeMap::new();
    };
    table
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
        .collect()
}

struct InvocationOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

fn spawn(repo_root: &Path, binary: &Path, args: &[String], invocation_id: &str) -> Result<InvocationOutput> {
    let mut command = Command::new(binary);
    command.args(args).current_dir(repo_root);
    let output = command.output().with_context(|| {
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
    needs_invocations: Option<&BTreeMap<String, String>>,
) -> Result<Vec<Finding>> {
    match invocation.exit.classify(output.exit_code) {
        ExitOutcome::Ok => Ok(Vec::new()),
        ExitOutcome::Findings => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            invocation
                .transform
                .apply(&output.stdout, output.exit_code, input_file, needs_invocations)
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

// ── Declarative fix executor ───────────────────────────────────────────────────

/// Outcome of executing one `fix` block from a declarative invocation inside
/// the T2 writable copy sandbox. Dropping the sandbox on error is implicit:
/// the [`WritableSandbox`] is owned locally and drops at the end of each
/// invocation's scope, so the real tree is untouched unless copy-back ran.
#[derive(Debug)]
pub struct FixInvocationOutcome {
    /// Invocation ID from the manifest, e.g. `"format"`.
    pub invocation_id: String,
    /// Repo-relative files atomically renamed into the real working tree (sorted).
    /// Each is a complete, valid file — the atomic rename guarantee from
    /// [`WritableSandbox::copy_back`] applies.
    pub applied: Vec<PathBuf>,
    /// Per-file errors when `fix.mode = per_file` (isolated failures). Each entry
    /// is `(file_path, error_message)` for a file whose fixer returned an error
    /// exit. Empty in batch mode and when all per-file runs succeed.
    pub per_file_errors: Vec<(PathBuf, String)>,
    /// Invocation-level error: binary resolution failure, spawn error, batch
    /// fixer error exit, or sandbox staging failure. On copy-back I/O error,
    /// this is also set — but `applied` may be non-empty in that case: those
    /// files were already atomically written to the real working tree before
    /// copy-back stopped. For all other error kinds, `applied` is empty and
    /// the real tree is untouched.
    pub error: Option<anyhow::Error>,
}

/// Execute all declared `fix` blocks for `package` over `fixable_files`.
///
/// Each invocation's `fix` block gets its own fresh [`WritableSandbox`]: the
/// staged files are force-copied (never hardlinked) so an in-place write by the
/// fixer cannot escape to the real tree. Only the files that actually changed in
/// the sandbox are copied back, via a same-directory temp + atomic rename.
///
/// `fixable_files` are repo-relative paths; files absent from `source_tree` are
/// silently dropped by staging. Only files that match the package's `applies_to`
/// globs (or the per-repo config override) are staged and passed to the fixer.
///
/// Invocations without a `fix` block are silently skipped. `bazel_aspect`
/// invocations likewise (they cannot declare a fix block by schema).
pub fn run_declarative_fix(
    repo_root: &Path,
    package: &ExternalCheckDeclarativePackage,
    fixable_files: &[PathBuf],
    source_tree: &dyn SourceTree,
    config: &toml::Value,
) -> Vec<FixInvocationOutcome> {
    let binaries = match resolve::resolve_all(repo_root, &package.needs, config) {
        Ok(b) => b,
        Err(err) => {
            // Binary resolution failed: synthesize an error for every fix-capable
            // invocation so the caller sees one entry per invocation that would
            // have run (mirrors the run path's per-invocation error surfacing).
            return package
                .invocations
                .iter()
                .filter(|inv| inv.fix.is_some())
                .map(|inv| FixInvocationOutcome {
                    invocation_id: inv.id.clone(),
                    applied: Vec::new(),
                    per_file_errors: Vec::new(),
                    error: Some(anyhow!("binary resolution failed: {err:#}")),
                })
                .collect();
        }
    };

    let config_values = extract_config_string_values(config);
    let ceiling = HostCeiling::new(repo_root);
    let mut outcomes = Vec::new();

    for invocation in &package.invocations {
        let Some(fix) = &invocation.fix else { continue };
        // bazel_aspect invocations cannot declare a fix block (enforced by schema validator)
        let InvocationKind::Tool(_) = &invocation.kind else {
            continue;
        };

        // Apply the applies_to override (if present in config) the same way the
        // check runner does: a per-repo config override replaces the definition's
        // glob list entirely.
        let applies_to_override = match resolve::override_applies_to(config) {
            Some(Ok(globs)) => Some(globs),
            Some(Err(err)) => {
                outcomes.push(FixInvocationOutcome {
                    invocation_id: invocation.id.clone(),
                    applied: Vec::new(),
                    per_file_errors: Vec::new(),
                    error: Some(anyhow!("invalid applies_to config override: {err:#}")),
                });
                continue;
            }
            None => None,
        };
        let applies_to: &[String] = applies_to_override.as_deref().unwrap_or(&package.applies_to);

        // Intersect fixable_files with the package's applies_to globs (or the config override).
        let filtered = match filter_by_applies_to(fixable_files, applies_to) {
            Ok(f) => f,
            Err(err) => {
                outcomes.push(FixInvocationOutcome {
                    invocation_id: invocation.id.clone(),
                    applied: Vec::new(),
                    per_file_errors: Vec::new(),
                    error: Some(err),
                });
                continue;
            }
        };
        if filtered.is_empty() {
            // No applicable files after filtering — record a no-op entry.
            outcomes.push(FixInvocationOutcome {
                invocation_id: invocation.id.clone(),
                applied: Vec::new(),
                per_file_errors: Vec::new(),
                error: None,
            });
            continue;
        }

        // Resolve the fix binary (defaults to the invocation's `run` binary).
        let binary = match binaries.get(&fix.run) {
            Some(b) => b,
            None => {
                outcomes.push(FixInvocationOutcome {
                    invocation_id: invocation.id.clone(),
                    applied: Vec::new(),
                    per_file_errors: Vec::new(),
                    error: Some(anyhow!(
                        "fix binary `{}` was not resolved (not declared in `needs`)",
                        fix.run
                    )),
                });
                continue;
            }
        };

        // Stage exactly `filtered` into a writable sandbox (force-copy; no hardlinks).
        let sandbox = match WritableSandbox::stage(&filtered, source_tree, &ceiling) {
            Ok(s) => s,
            Err(err) => {
                outcomes.push(FixInvocationOutcome {
                    invocation_id: invocation.id.clone(),
                    applied: Vec::new(),
                    per_file_errors: Vec::new(),
                    error: Some(anyhow!("failed to stage writable fix sandbox: {err:#}")),
                });
                continue;
            }
        };

        // The staged paths are the subset of `filtered` present in the source tree.
        // Any absent paths were silently skipped by staging.
        let staged = sandbox.staged_paths();
        if staged.is_empty() {
            outcomes.push(FixInvocationOutcome {
                invocation_id: invocation.id.clone(),
                applied: Vec::new(),
                per_file_errors: Vec::new(),
                error: None,
            });
            continue;
        }
        let staged_strings: Vec<String> = staged.iter().map(|p| p.to_string_lossy().into_owned()).collect();

        let outcome = match fix.mode {
            InvocationMode::Batch => execute_fix_batch(
                sandbox.root_path(),
                repo_root,
                &binary.program,
                &binary.prefix_args,
                fix,
                &staged_strings,
                &config_values,
                &invocation.id,
                &sandbox,
            ),
            InvocationMode::PerFile => execute_fix_per_file(
                sandbox.root_path(),
                repo_root,
                &binary.program,
                &binary.prefix_args,
                fix,
                &staged_strings,
                &config_values,
                &invocation.id,
                &sandbox,
            ),
        };

        outcomes.push(outcome.unwrap_or_else(|err| FixInvocationOutcome {
            invocation_id: invocation.id.clone(),
            applied: Vec::new(),
            per_file_errors: Vec::new(),
            error: Some(err),
        }));
    }

    outcomes
}

/// Run a batch fix invocation: one process over all staged files (chunked when
/// necessary to stay under ARG_MAX). Any chunk error aborts the entire invocation
/// — the sandbox is dropped without copy-back, leaving the real tree untouched.
///
/// # cwd and config-file discovery
///
/// The fixer runs with `cwd = sandbox_root`. Config-discovering tools (biome,
/// prettier, eslint, rustfmt) walk up from cwd to find their config files, but
/// only the fixable files — not repo-root config files — are staged. A formatter
/// fix block will therefore run with DEFAULT config and may write mis-formatted
/// bytes back to the real tree. Until fix blocks stage config files alongside
/// fixable files, formatter fix args must pass `--config {{repo_root}}/...` explicitly.
#[allow(clippy::too_many_arguments)]
fn execute_fix_batch(
    sandbox_root: &Path,
    repo_root: &Path,
    program: &Path,
    prefix_args: &[String],
    fix: &FixBlock,
    files: &[String],
    config_values: &BTreeMap<String, String>,
    invocation_id: &str,
    sandbox: &WritableSandbox,
) -> Result<FixInvocationOutcome> {
    let fixed_cost = argv_byte_cost_of_path(program)
        + argv_byte_cost(prefix_args)
        + fix
            .args
            .iter()
            .filter(|a| *a != "{{files}}")
            .map(|a| a.replace("{{repo_root}}", &repo_root.to_string_lossy()))
            .map(|a| a.len() + 1)
            .sum::<usize>();
    let chunks = split_files_into_chunks(fixed_cost, files);

    for chunk in chunks {
        let mut args = prefix_args.to_vec();
        args.extend(expand_batch_args(repo_root, &fix.args, chunk, config_values)?);
        let output = spawn(sandbox_root, program, &args, invocation_id)?;
        if fix.exit.classify(output.exit_code) == FixExitOutcome::Error {
            // Any chunk error → abort. Dropping `sandbox` removes staged work;
            // the real tree is untouched.
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(FixInvocationOutcome {
                invocation_id: invocation_id.to_owned(),
                applied: Vec::new(),
                per_file_errors: Vec::new(),
                error: Some(anyhow!(
                    "fix invocation `{invocation_id}` exited with status {} (treated as error): {}",
                    describe_exit(output.exit_code),
                    stderr.trim()
                )),
            });
        }
    }

    // All chunks succeeded — detect which staged files changed and copy them back.
    let changed = sandbox
        .detect_changes()
        .context("failed to detect changes in fix sandbox")?;
    let report = sandbox.copy_back(&changed, repo_root);
    let error = report
        .failed
        .map(|(path, err)| anyhow!("copy-back stopped at {}: {err:#}", path.display()));
    Ok(FixInvocationOutcome {
        invocation_id: invocation_id.to_owned(),
        applied: report.applied,
        per_file_errors: Vec::new(),
        error,
    })
}

/// Run a per-file fix invocation: one process per staged file. Errors are
/// isolated per file (one file's error does not abort the others). After all
/// per-file runs, only the files whose fixer succeeded AND whose content changed
/// are copied back.
///
/// # cwd and config-file discovery
///
/// See the note on [`execute_fix_batch`]: the same cwd=sandbox/config-discovery
/// concern applies here. Formatter fix args must pass config explicitly via
/// `--config {{repo_root}}/...` until sandbox staging includes config files.
#[allow(clippy::too_many_arguments)]
fn execute_fix_per_file(
    sandbox_root: &Path,
    repo_root: &Path,
    program: &Path,
    prefix_args: &[String],
    fix: &FixBlock,
    files: &[String],
    config_values: &BTreeMap<String, String>,
    invocation_id: &str,
    sandbox: &WritableSandbox,
) -> Result<FixInvocationOutcome> {
    let mut ok_files: HashSet<PathBuf> = HashSet::new();
    let mut per_file_errors: Vec<(PathBuf, String)> = Vec::new();

    for file in files {
        let mut args = prefix_args.to_vec();
        args.extend(expand_per_file_args(repo_root, &fix.args, file, config_values)?);
        match spawn(sandbox_root, program, &args, invocation_id) {
            Ok(output) => match fix.exit.classify(output.exit_code) {
                FixExitOutcome::Ok => {
                    ok_files.insert(PathBuf::from(file));
                }
                FixExitOutcome::Error => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let msg = stderr.trim();
                    per_file_errors.push((
                        PathBuf::from(file),
                        if msg.is_empty() {
                            format!("exit {}", describe_exit(output.exit_code))
                        } else {
                            msg.to_owned()
                        },
                    ));
                }
            },
            Err(err) => {
                per_file_errors.push((PathBuf::from(file), err.to_string()));
            }
        }
    }

    // Detect which staged files changed, then copy back only those where the
    // fixer returned ok. Files that errored stay at their original bytes.
    let changed = sandbox
        .detect_changes()
        .context("failed to detect changes in fix sandbox")?;
    let to_copy_back: Vec<PathBuf> = changed.into_iter().filter(|p| ok_files.contains(p)).collect();
    let report = sandbox.copy_back(&to_copy_back, repo_root);
    let error = report
        .failed
        .map(|(path, err)| anyhow!("copy-back stopped at {}: {err:#}", path.display()));
    Ok(FixInvocationOutcome {
        invocation_id: invocation_id.to_owned(),
        applied: report.applied,
        per_file_errors,
        error,
    })
}

/// Filter `files` to those whose repo-relative path matches any of the
/// `applies_to` glob patterns. Returns the matching subset (same order).
fn filter_by_applies_to(files: &[PathBuf], applies_to: &[String]) -> Result<Vec<PathBuf>> {
    let mut builder = GlobSetBuilder::new();
    for pattern in applies_to {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid applies_to glob `{pattern}`"))?);
    }
    let globset = builder.build().context("failed to build applies_to glob set")?;
    Ok(files.iter().filter(|p| globset.is_match(p)).cloned().collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::{TempDir, tempdir};

    use super::{execute_fix_batch, execute_fix_per_file};
    use crate::external::declarative::FixBlock;
    use crate::external::sandbox::HostCeiling;
    use crate::fix::safety::WritableSandbox;
    use crate::source_tree::LocalSourceTree;

    fn paths(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    fn disk_tree(entries: &[(&str, &[u8])]) -> (TempDir, LocalSourceTree) {
        let dir = tempdir().expect("temp dir");
        for (path, content) in entries {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create dirs");
            }
            fs::write(&full, content).expect("write file");
        }
        let tree = LocalSourceTree::new(dir.path()).expect("create tree");
        (dir, tree)
    }

    /// Parse a minimal manifest to obtain a [`FixBlock`] with standard exit
    /// semantics (0 = ok, else = error) for the given `mode` and `args`.
    fn make_fix_block(mode_str: &str, args: &[&str]) -> FixBlock {
        use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
        let args_yaml = args
            .iter()
            .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ");
        // The check invocation uses batch/{{files}} regardless; only the fix block's
        // mode and args matter for our executor tests.
        let yaml = format!(
            r#"
id: test/fix
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**"]
needs:
  bin:
    default:
      path: /bin/true
invocations:
  - id: inv
    run: bin
    mode: batch
    args: ["{{{{files}}}}"]
    exit:
      "0": ok
      default: error
    transform:
      kind: linelist
      message: "x"
    fix:
      mode: {mode_str}
      args: [{args_yaml}]
"#
        );
        let pkg = parse_declarative_check_manifest(&yaml).expect("parse test fix manifest");
        match pkg.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => {
                d.invocations[0].fix.clone().expect("fix block present")
            }
            _ => panic!("expected declarative"),
        }
    }

    /// Create an executable shell script at `dir/name` and return its path.
    #[cfg(unix)]
    fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
        path
    }

    /// Per-file mode: when file A succeeds (with a change) and file B errors,
    /// only A is copied back to the real tree; B stays at its original bytes.
    #[cfg(unix)]
    #[test]
    fn per_file_ok_and_error_isolates() {
        let (dir, tree) = disk_tree(&[("a.txt", b"before"), ("b.txt", b"before")]);
        let scripts_dir = tempdir().expect("scripts dir");
        // Rewrites the file to "FIXED" then exits 0 for a.txt, 1 for b.txt.
        let script = make_script(
            scripts_dir.path(),
            "fixer.sh",
            r#"printf 'FIXED' > "$1"
case "$1" in *b.txt) exit 1 ;; *) exit 0 ;; esac"#,
        );

        let ceiling = HostCeiling::new(dir.path());
        let sandbox = WritableSandbox::stage(&paths(&["a.txt", "b.txt"]), &tree, &ceiling).expect("stage");
        let staged: Vec<String> = sandbox
            .staged_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let fix = make_fix_block("per_file", &["{{file}}"]);
        // Run via interpreter so no thread holds a write fd to a file being execve'd.
        let prefix = vec![script.to_string_lossy().into_owned()];

        let outcome = execute_fix_per_file(
            sandbox.root_path(),
            dir.path(),
            Path::new("/bin/sh"),
            &prefix,
            &fix,
            &staged,
            &BTreeMap::new(),
            "inv",
            &sandbox,
        )
        .expect("execute");

        assert_eq!(
            outcome.applied,
            paths(&["a.txt"]),
            "only the ok+changed file is copied back"
        );
        assert_eq!(outcome.per_file_errors.len(), 1, "b.txt should have a per-file error");
        assert_eq!(outcome.per_file_errors[0].0, PathBuf::from("b.txt"));
        assert!(outcome.error.is_none());
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"FIXED",
            "a.txt updated in real tree"
        );
        assert_eq!(
            fs::read(dir.path().join("b.txt")).unwrap(),
            b"before",
            "b.txt unchanged in real tree"
        );
    }

    /// Per-file mode: when the fixer modifies the sandbox copy but exits with
    /// error, the change must be discarded and the real tree must be untouched.
    #[cfg(unix)]
    #[test]
    fn per_file_changed_but_errored_discards_change() {
        let (dir, tree) = disk_tree(&[("a.txt", b"before")]);
        let scripts_dir = tempdir().expect("scripts dir");
        // Always rewrites the file AND always exits 1.
        let script = make_script(
            scripts_dir.path(),
            "fixer.sh",
            r#"printf 'FIXED' > "$1"
exit 1"#,
        );

        let ceiling = HostCeiling::new(dir.path());
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &ceiling).expect("stage");
        let staged: Vec<String> = sandbox
            .staged_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let fix = make_fix_block("per_file", &["{{file}}"]);
        let prefix = vec![script.to_string_lossy().into_owned()];

        let outcome = execute_fix_per_file(
            sandbox.root_path(),
            dir.path(),
            Path::new("/bin/sh"),
            &prefix,
            &fix,
            &staged,
            &BTreeMap::new(),
            "inv",
            &sandbox,
        )
        .expect("execute");

        assert!(
            outcome.applied.is_empty(),
            "errored file must not be copied back even if sandbox changed"
        );
        assert_eq!(outcome.per_file_errors.len(), 1);
        assert!(outcome.error.is_none());
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"before",
            "real tree unchanged"
        );
    }

    /// Batch mode: when the fixer exits with error, no copy-back occurs and the
    /// real tree is left untouched.
    #[cfg(unix)]
    #[test]
    fn batch_error_leaves_real_tree_untouched() {
        let (dir, tree) = disk_tree(&[("a.txt", b"before")]);
        let scripts_dir = tempdir().expect("scripts dir");
        // Always exits 1 without touching any files.
        let script = make_script(scripts_dir.path(), "fixer.sh", "exit 1");

        let ceiling = HostCeiling::new(dir.path());
        let sandbox = WritableSandbox::stage(&paths(&["a.txt"]), &tree, &ceiling).expect("stage");
        let staged: Vec<String> = sandbox
            .staged_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let fix = make_fix_block("batch", &["{{files}}"]);
        let prefix = vec![script.to_string_lossy().into_owned()];

        let outcome = execute_fix_batch(
            sandbox.root_path(),
            dir.path(),
            Path::new("/bin/sh"),
            &prefix,
            &fix,
            &staged,
            &BTreeMap::new(),
            "inv",
            &sandbox,
        )
        .expect("execute returns Ok(outcome) with error field set");

        assert!(outcome.applied.is_empty(), "no files must be applied on batch error");
        assert!(outcome.per_file_errors.is_empty());
        assert!(outcome.error.is_some(), "invocation-level error must be recorded");
        assert_eq!(
            fs::read(dir.path().join("a.txt")).unwrap(),
            b"before",
            "real tree untouched"
        );
    }
}
