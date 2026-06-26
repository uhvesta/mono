use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use tar::Archive;
use tokio::task::JoinSet;

use crate::progress::{NoopProgressReporter, ProgressReporter, files_failed_count};

use crate::bypass::{bypass_applied_finding, bypass_failure_guidance, bypass_name_for_check_id};
use crate::check::{CheckRegistry, ConfiguredCheck};
use crate::config::{
    CheckConfig, CheckConfigOrigin, ConfigDiagnostic, ConfigResolver, StarlarkPackageActivation, StarlarkPackageConfig,
};
use crate::exclusion::ExclusionStatus;
use crate::exclusion_matcher::ExclusionMatcher;
use crate::external::{
    ExternalCheckExecutor, ExternalCheckPackage, ExternalCheckPackageImplementation, ExternalCheckPackageProvider,
    NoopExternalCheckExecutor, NoopExternalCheckPackageProvider,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::starlark::adapter::{AdapterInput, AdapterPreparedOutput, AdapterRegistry, FormatAdapter};
use crate::starlark::discovery::{self, DiscoveredCheck};
use crate::starlark::manifest::{PackageKind, PackageManifest, PackageRef};
use crate::starlark::{StarlarkCheckRunner, StarlarkCheckSource};
use tracing::info;

struct ScheduledCheckRun {
    configured_check_id: String,
    source_path: PathBuf,
    execution: ScheduledExecution,
    policy: EffectiveCheckPolicy,
    config: toml::Value,
    changeset: ChangeSet,
    /// Effective exclusion matcher for this check instance (global ∪ per-check
    /// excludes). Used to subtract excluded paths from what the check sees and to
    /// back-stop any finding that lands on an excluded path.
    exclusion_matcher: ExclusionMatcher,
}

enum ScheduledExecution {
    BuiltInConfigured {
        check: Arc<dyn ConfiguredCheck>,
    },
    BuiltInMissing {
        implementation_check_id: String,
    },
    ExternalResolved {
        package: Box<ExternalCheckPackage>,
    },
    StarlarkLocal {
        check: Arc<StarlarkCheckRunner>,
        output: Arc<AdapterPreparedOutput>,
        package_tree: Arc<dyn SourceTree>,
        fix_path: Option<PathBuf>,
        checkleft_root: PathBuf,
        check_dir: PathBuf,
    },
    Invalid {
        message: String,
        remediation: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct EffectiveCheckPolicy {
    severity_override: Option<Severity>,
    allow_bypass: bool,
    bypass_name: String,
    /// When true, `apply_policy_to_result` preserves per-finding severity set by
    /// the transform instead of defaulting to Error. Set for declarative checks
    /// that opt into dynamic severity via `SeverityTemplate::Dynamic`. An explicit
    /// `severity_override` still takes precedence and flattens all findings.
    preserve_finding_severity: bool,
}

impl EffectiveCheckPolicy {
    fn fingerprint(&self) -> String {
        format!(
            "severity={:?};allow_bypass={};bypass_name={}",
            self.severity_override, self.allow_bypass, self.bypass_name
        )
    }
}

#[derive(Default)]
struct ScheduledRuns {
    runs: Vec<ScheduledCheckRun>,
    diagnostics: Vec<CheckResult>,
}

#[derive(Debug, Clone)]
struct SelectedStarlarkPackage {
    package: StarlarkPackageConfig,
    explicit_check_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedPackageRef {
    source: String,
    version: String,
    sha256: Option<String>,
}

struct ResolvedStarlarkPackage {
    root: PathBuf,
    tree: Arc<dyn SourceTree>,
}

/// The number of fix passes `dispatch_fix` applies when `--max-passes` is not
/// supplied on the command line. Must be ≥ 2 so a formatter that requires two
/// passes to reach a stable state converges in a single `checkleft fix --all`
/// invocation.
pub const DEFAULT_FIX_PASSES: u32 = 10;

pub struct Runner {
    registry: Arc<CheckRegistry>,
    resolver: Arc<ConfigResolver>,
    source_tree: Arc<dyn SourceTree>,
    external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
    external_executor: Arc<dyn ExternalCheckExecutor>,
}

impl Runner {
    pub fn new(registry: Arc<CheckRegistry>, resolver: Arc<ConfigResolver>, source_tree: Arc<dyn SourceTree>) -> Self {
        Self::with_external(
            registry,
            resolver,
            source_tree,
            Arc::new(NoopExternalCheckPackageProvider),
            Arc::new(NoopExternalCheckExecutor),
        )
    }

    pub fn with_external_package_provider(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
        external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
    ) -> Self {
        Self::with_external(
            registry,
            resolver,
            source_tree,
            external_package_provider,
            Arc::new(NoopExternalCheckExecutor),
        )
    }

    pub fn with_external(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
        external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
        external_executor: Arc<dyn ExternalCheckExecutor>,
    ) -> Self {
        Self {
            registry,
            resolver,
            source_tree,
            external_package_provider,
            external_executor,
        }
    }

    pub async fn run_changeset(&self, changeset: &ChangeSet) -> Result<Vec<CheckResult>> {
        // The non-interactive path: a no-op reporter, so behavior and output are
        // identical to a build without the progress UI.
        self.run_changeset_with_progress(changeset, Arc::new(NoopProgressReporter))
            .await
    }

    /// Like [`Self::run_changeset`] but emits per-check lifecycle events to
    /// `reporter` for the interactive progress UI. The reporter is
    /// presentation-only: it never affects scheduling, findings, or ordering of
    /// the returned results. Each executing check registers up front, then emits
    /// `start` / `finish` from its own task (checks run concurrently, hence the
    /// thread-safe `Arc<dyn ProgressReporter>`); findings stream into the log
    /// area as each check completes.
    pub async fn run_changeset_with_progress(
        &self,
        changeset: &ChangeSet,
        reporter: Arc<dyn ProgressReporter>,
    ) -> Result<Vec<CheckResult>> {
        let scheduled = self.schedule_runs(changeset)?;
        info!(
            scheduled_runs = scheduled.runs.len(),
            diagnostics = scheduled.diagnostics.len(),
            "scheduled check execution"
        );

        let mut results = scheduled.diagnostics;
        // Config diagnostics are produced synchronously (no run); stream them so
        // they appear in the log area alongside the checks that did run.
        for result in &results {
            reporter.stream_findings(result);
        }
        let mut join_set = JoinSet::new();
        for run in scheduled.runs {
            match run.execution {
                ScheduledExecution::BuiltInConfigured { check } => {
                    let source_tree = Arc::clone(&self.source_tree);
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_policy = run.policy;
                    let source_path = run.source_path;
                    let exclusion_matcher = run.exclusion_matcher;
                    // Built-in Rust checks receive the same exclusion-filtered view the
                    // host lowers into component checks: an excluded path is removed
                    // before the check runs, so it is never triggered on that path.
                    let check_changeset = exclusion_matcher.filter_changeset(&run_changeset);
                    let file_count = check.applicable_file_count(&check_changeset);
                    info!(
                        check_id = %configured_check_id,
                        file_count,
                        "running built-in check"
                    );
                    reporter.register(&configured_check_id, file_count);
                    let reporter = Arc::clone(&reporter);

                    join_set.spawn(async move {
                        reporter.start(&configured_check_id);
                        let check_start = Instant::now();
                        let progress_reporter = Arc::clone(&reporter);
                        let progress_check_id = configured_check_id.clone();
                        let on_file_processed: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |n| {
                            progress_reporter.record_progress(&progress_check_id, n);
                        });
                        match check
                            .run_with_progress(&check_changeset, source_tree.as_ref(), on_file_processed)
                            .await
                        {
                            Ok(mut result) => {
                                let elapsed = check_start.elapsed();
                                // Report findings under the configured instance id.
                                result.check_id = configured_check_id.clone();
                                let result =
                                    apply_policy_to_result(result, &run_policy, &run_changeset, &exclusion_matcher);
                                info!(
                                    check_id = %configured_check_id,
                                    elapsed_ms = elapsed.as_millis(),
                                    findings = result.findings.len(),
                                    "built-in check complete"
                                );
                                reporter.stream_findings(&result);
                                reporter.finish(&configured_check_id, files_failed_count(&result), elapsed);
                                Ok(result)
                            }
                            Err(err) => {
                                reporter.finish(&configured_check_id, 1, check_start.elapsed());
                                Err((configured_check_id, source_path, err))
                            }
                        }
                    });
                }
                ScheduledExecution::BuiltInMissing {
                    implementation_check_id,
                } => {
                    let result = CheckResult {
                        check_id: run.configured_check_id,
                        findings: vec![Finding {
                            severity: Severity::Error,
                            message: format!(
                                "configured check references unknown implementation `{implementation_check_id}`"
                            ),
                            location: Some(Location {
                                path: run.source_path.clone(),
                                line: None,
                                column: None,
                            }),
                            remediations: vec![
                                "Register this check implementation in the binary or fix `check = ...` in CHECKS.yaml."
                                    .to_owned(),
                            ],
                            suggested_fix: None,
                        }],
                    };
                    reporter.stream_findings(&result);
                    results.push(result);
                }
                ScheduledExecution::ExternalResolved { package } => {
                    let external_executor = Arc::clone(&self.external_executor);
                    let source_tree = Arc::clone(&self.source_tree);
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_config = run.config;
                    let run_policy = run.policy;
                    let source_path = run.source_path;
                    let run_config_dir = source_path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(""))
                        .to_path_buf();
                    let exclusion_matcher = run.exclusion_matcher;
                    // Seed the progress count from the exclusion-filtered view so it
                    // matches what the executor will actually process.
                    let file_count = self.external_executor.eligible_file_count(
                        &package,
                        &exclusion_matcher.filter_changeset(&run_changeset),
                        &run_config,
                    );
                    info!(
                        check_id = %configured_check_id,
                        package_id = %package.id,
                        file_count,
                        "running external check"
                    );
                    reporter.register(&configured_check_id, file_count);
                    let reporter = Arc::clone(&reporter);

                    join_set.spawn(async move {
                        reporter.start(&configured_check_id);
                        // The executor is synchronous but wasmtime-wasi internally calls
                        // block_on, which panics if a Tokio runtime is already active on
                        // the thread.  spawn_blocking moves execution onto a thread-pool
                        // thread where no runtime is running.
                        let check_id_clone = configured_check_id.clone();
                        let source_path_clone = source_path.clone();
                        let task_reporter = Arc::clone(&reporter);
                        let outcome = tokio::task::spawn_blocking(move || {
                            let check_start = Instant::now();
                            let progress_reporter = Arc::clone(&task_reporter);
                            let progress_check_id = configured_check_id.clone();
                            let on_file_processed: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |n| {
                                progress_reporter.record_progress(&progress_check_id, n);
                            });
                            match external_executor.execute_with_progress(
                                &package,
                                &run_changeset,
                                source_tree.as_ref(),
                                &run_config,
                                &run_config_dir,
                                run_policy.severity_override,
                                &exclusion_matcher,
                                on_file_processed,
                            ) {
                                Ok(mut result) => {
                                    let elapsed = check_start.elapsed();
                                    result.check_id = configured_check_id.clone();
                                    let result =
                                        apply_policy_to_result(result, &run_policy, &run_changeset, &exclusion_matcher);
                                    info!(
                                        check_id = %configured_check_id,
                                        elapsed_ms = elapsed.as_millis(),
                                        findings = result.findings.len(),
                                        "external check complete"
                                    );
                                    task_reporter.stream_findings(&result);
                                    task_reporter.finish(&configured_check_id, files_failed_count(&result), elapsed);
                                    Ok(result)
                                }
                                Err(err) => {
                                    task_reporter.finish(&configured_check_id, 1, check_start.elapsed());
                                    Err((configured_check_id, source_path, err))
                                }
                            }
                        })
                        .await;
                        outcome.unwrap_or_else(|e| {
                            reporter.finish(&check_id_clone, 1, Duration::ZERO);
                            Err((check_id_clone, source_path_clone, anyhow!("executor panicked: {e}")))
                        })
                    });
                }
                ScheduledExecution::StarlarkLocal {
                    check,
                    output,
                    package_tree,
                    ..
                } => {
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_policy = run.policy;
                    let source_path = run.source_path;
                    let exclusion_matcher = run.exclusion_matcher;
                    let file_count = run_changeset.changed_files.len();
                    info!(
                        check_id = %configured_check_id,
                        file_count,
                        "running local Starlark check"
                    );
                    reporter.register(&configured_check_id, file_count);
                    let reporter = Arc::clone(&reporter);

                    join_set.spawn(async move {
                        reporter.start(&configured_check_id);
                        let check_start = Instant::now();
                        match check.evaluate_prepared_adapter(output.as_ref(), package_tree.as_ref()) {
                            Ok(mut result) => {
                                let elapsed = check_start.elapsed();
                                result.check_id = configured_check_id.clone();
                                let result =
                                    apply_policy_to_result(result, &run_policy, &run_changeset, &exclusion_matcher);
                                info!(
                                    check_id = %configured_check_id,
                                    elapsed_ms = elapsed.as_millis(),
                                    findings = result.findings.len(),
                                    "local Starlark check complete"
                                );
                                reporter.stream_findings(&result);
                                reporter.finish(&configured_check_id, files_failed_count(&result), elapsed);
                                Ok(result)
                            }
                            Err(err) => {
                                reporter.finish(&configured_check_id, 1, check_start.elapsed());
                                Err((configured_check_id, source_path, err))
                            }
                        }
                    });
                }
                ScheduledExecution::Invalid { message, remediation } => {
                    let result = CheckResult {
                        check_id: run.configured_check_id,
                        findings: vec![Finding {
                            severity: Severity::Error,
                            message,
                            location: Some(Location {
                                path: run.source_path.clone(),
                                line: None,
                                column: None,
                            }),
                            remediations: remediation.into_iter().collect(),
                            suggested_fix: None,
                        }],
                    };
                    reporter.stream_findings(&result);
                    results.push(result);
                }
            }
        }

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(Ok(result)) => results.push(result),
                Ok(Err((check_id, source_path, err))) => {
                    let result = CheckResult {
                        check_id,
                        findings: vec![Finding {
                            severity: Severity::Error,
                            message: format!("check execution failed: {err:#}"),
                            location: Some(Location {
                                path: source_path,
                                line: None,
                                column: None,
                            }),
                            remediations: vec![],
                            suggested_fix: None,
                        }],
                    };
                    // The status line was already marked failed from the task; stream
                    // the rendered execution-failure finding into the log area.
                    reporter.stream_findings(&result);
                    results.push(result);
                }
                Err(join_err) => {
                    return Err(anyhow!("runner task failed to execute: {join_err}"));
                }
            }
        }

        let audit = self.audit_stale_exclusions(changeset).await?;
        for result in &audit {
            reporter.stream_findings(result);
        }
        results.extend(audit);

        results.sort_by(|left, right| left.check_id.cmp(&right.check_id));
        Ok(results)
    }

    /// Diff-gated stale-exclusion audit (see [`crate::exclusion`]).
    ///
    /// Unlike the normal pass, this reports on `CHECKS` files that did *not*
    /// change: an exclusion goes stale because a file it depends on changed. For every
    /// changed file we resolve the checks that govern it, ask each built-in check for
    /// its declared exclusions, and re-evaluate only those whose declared dependencies
    /// intersect the changeset. A `Stale` verdict becomes a finding anchored on the
    /// exclusion's line in the owning `CHECKS` file.
    async fn audit_stale_exclusions(&self, changeset: &ChangeSet) -> Result<Vec<CheckResult>> {
        // Every path the diff touches, including deletions and rename sources: a stale
        // exclusion is triggered by a change to a file it depends on, and a deletion is
        // exactly the dangling-reference case the audit must catch.
        let mut changed_paths: HashSet<PathBuf> = HashSet::new();
        for changed_file in &changeset.changed_files {
            changed_paths.insert(changed_file.path.clone());
            if let Some(old_path) = &changed_file.old_path {
                changed_paths.insert(old_path.clone());
            }
        }
        if changed_paths.is_empty() {
            return Ok(Vec::new());
        }

        // De-duplicate (check instance, owning config, entry): the same exclusion is
        // reachable through every changed file it depends on, but must be audited once.
        let mut audited: HashSet<(String, PathBuf, String)> = HashSet::new();
        // Cache CHECKS file contents read to locate entry lines.
        let mut config_text_cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut findings_by_check: BTreeMap<String, Vec<Finding>> = BTreeMap::new();

        for changed_file in &changeset.changed_files {
            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            let default_mode = resolved.stale_exclusion_mode();

            for check in resolved.enabled() {
                let mode = check.policy.stale_exclusion_mode.unwrap_or(default_mode);
                let Some(severity) = mode.severity() else {
                    continue; // audit disabled for this check
                };
                if let Some(built_in) = self.registry.get(&check.check) {
                    // Native built-in check path.
                    let Ok(configured) = built_in.configure_scoped(&check.config, check.source_path.parent()) else {
                        // Misconfiguration is surfaced by the normal pass; stay quiet here.
                        continue;
                    };
                    let declared = configured.declared_exclusions();
                    if declared.is_empty() {
                        continue;
                    }

                    for exclusion in declared {
                        if exclusion.depends_on.is_empty() {
                            continue;
                        }
                        if !exclusion
                            .depends_on
                            .iter()
                            .any(|dependency| changed_paths.contains(dependency))
                        {
                            continue;
                        }
                        let dedupe_key = (check.id.clone(), check.source_path.clone(), exclusion.entry.clone());
                        if !audited.insert(dedupe_key) {
                            continue;
                        }

                        let status = match configured
                            .evaluate_exclusion(&exclusion, self.source_tree.as_ref())
                            .await
                        {
                            Ok(status) => status,
                            Err(err) => {
                                info!(
                                    check_id = %check.id,
                                    entry = %exclusion.entry,
                                    error = %err,
                                    "stale-exclusion audit failed; skipping entry"
                                );
                                continue;
                            }
                        };
                        let ExclusionStatus::Stale { reason } = status else {
                            continue;
                        };

                        let line =
                            self.locate_exclusion_line(&check.source_path, &exclusion.entry, &mut config_text_cache);
                        findings_by_check.entry(check.id.clone()).or_default().push(Finding {
                            severity,
                            message: format!(
                                "exclusion `{}` is no longer needed: {reason}; remove this entry.",
                                exclusion.entry
                            ),
                            location: Some(Location {
                                path: check.source_path.clone(),
                                line,
                                column: None,
                            }),
                            remediations: vec![format!(
                                "Remove `{}` from this check's exclusions in {}.",
                                exclusion.entry,
                                check.source_path.display()
                            )],
                            suggested_fix: None,
                        });
                    }
                } else if let Some(impl_ref) = &check.implementation {
                    // Component (wasm) check path.
                    let package = match self.external_package_provider.resolve(impl_ref) {
                        Ok(Some(pkg)) => pkg,
                        _ => continue,
                    };
                    let ExternalCheckPackageImplementation::Component(_) = &package.implementation else {
                        continue;
                    };
                    let config_json = match serde_json::to_string(&check.config) {
                        Ok(j) => j,
                        Err(err) => {
                            info!(check_id = %check.id, error = %err, "failed to serialize config for exclusion audit");
                            continue;
                        }
                    };

                    let declared = match self.external_executor.declared_exclusions_for_component(
                        &package,
                        &check.check,
                        &config_json,
                        &check.config_dir,
                    ) {
                        Ok(d) => d,
                        Err(err) => {
                            info!(check_id = %check.id, error = %err, "declared-exclusions call failed; skipping check");
                            continue;
                        }
                    };
                    if declared.is_empty() {
                        continue;
                    }

                    for exclusion in declared {
                        if exclusion.depends_on.is_empty() {
                            continue;
                        }
                        if !exclusion
                            .depends_on
                            .iter()
                            .any(|dependency| changed_paths.contains(dependency))
                        {
                            continue;
                        }
                        let dedupe_key = (check.id.clone(), check.source_path.clone(), exclusion.entry.clone());
                        if !audited.insert(dedupe_key) {
                            continue;
                        }

                        // Read the content of the first depended-on file (fail-safe: None = deleted).
                        let file_content_bytes = exclusion
                            .depends_on
                            .first()
                            .and_then(|dep| self.source_tree.read_file(dep).ok());
                        let file_content_str: Option<String> =
                            file_content_bytes.and_then(|bytes| String::from_utf8(bytes).ok());

                        let status = match self.external_executor.evaluate_exclusion_for_component(
                            &package,
                            &check.check,
                            &config_json,
                            &exclusion,
                            file_content_str.as_deref(),
                        ) {
                            Ok(s) => s,
                            Err(err) => {
                                info!(
                                    check_id = %check.id,
                                    entry = %exclusion.entry,
                                    error = %err,
                                    "stale-exclusion audit failed; skipping entry"
                                );
                                continue;
                            }
                        };
                        let ExclusionStatus::Stale { reason } = status else {
                            continue;
                        };

                        let line =
                            self.locate_exclusion_line(&check.source_path, &exclusion.entry, &mut config_text_cache);
                        findings_by_check.entry(check.id.clone()).or_default().push(Finding {
                            severity,
                            message: format!(
                                "exclusion `{}` is no longer needed: {reason}; remove this entry.",
                                exclusion.entry
                            ),
                            location: Some(Location {
                                path: check.source_path.clone(),
                                line,
                                column: None,
                            }),
                            remediations: vec![format!(
                                "Remove `{}` from this check's exclusions in {}.",
                                exclusion.entry,
                                check.source_path.display()
                            )],
                            suggested_fix: None,
                        });
                    }
                }
            }
        }

        Ok(findings_by_check
            .into_iter()
            .map(|(check_id, findings)| CheckResult { check_id, findings })
            .collect())
    }

    /// Find the 1-based line of `entry` in the `CHECKS` file at `source_path`, reading
    /// (and caching) the file through the source tree. Returns `None` if the file can't
    /// be read or the entry text isn't found — the finding then points at the file
    /// without a line.
    fn locate_exclusion_line(
        &self,
        source_path: &Path,
        entry: &str,
        cache: &mut HashMap<PathBuf, Option<String>>,
    ) -> Option<u32> {
        let contents = cache
            .entry(source_path.to_path_buf())
            .or_insert_with(|| {
                self.source_tree
                    .read_file(source_path)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            })
            .as_ref()?;
        locate_entry_line(contents, entry)
    }

    /// Execute declared `fix` blocks for every declarative check whose ID appears
    /// in `fix_plan`. Checks not in `fix_plan` are skipped entirely.
    ///
    /// Disjoint check groups (checks whose fixable-file sets do not overlap) run
    /// concurrently via rayon. Checks within a group are applied serially in
    /// category order (lint before format) so each check sees the latest bytes
    /// written by its predecessor. The pipeline repeats until convergence or
    /// `max_passes` is hit; a pass with no files written terminates early.
    ///
    /// Returns a map from configured check ID to per-invocation outcomes across
    /// all passes. A check that is not a declarative external check (built-in,
    /// WASM) maps to an empty `Vec` (no declarative fix available). A declarative
    /// check with no `fix` blocks on any invocation likewise maps to an empty `Vec`.
    ///
    /// `reporter` receives lifecycle events for the apply phase (register/start/
    /// record_progress/finish per check), identical in shape to the run phase so
    /// the same `LiveProgress` instance can be reused across both phases.
    pub fn run_declarative_fixes(
        &self,
        changeset: &ChangeSet,
        fix_plan: &BTreeMap<String, Vec<PathBuf>>,
        repo_root: &Path,
        max_passes: u32,
        reporter: Arc<dyn ProgressReporter>,
    ) -> Result<BTreeMap<String, Vec<crate::external::FixInvocationOutcome>>> {
        use rayon::prelude::*;

        let scheduled = self.schedule_runs(changeset)?;

        // Collect the declarative package + per-check config + exclusion matcher for
        // each check in the fix plan. Non-declarative checks (built-in, WASM) map to None.
        let mut declarative_info: BTreeMap<
            String,
            Option<(
                crate::external::ExternalCheckDeclarativePackage,
                toml::Value,
                ExclusionMatcher,
            )>,
        > = BTreeMap::new();
        for run in scheduled.runs {
            let check_id = run.configured_check_id.clone();
            if !fix_plan.contains_key(&check_id) {
                continue;
            }
            let exclusion_matcher = run.exclusion_matcher.clone();
            declarative_info
                .entry(check_id)
                .or_insert_with(|| match &run.execution {
                    ScheduledExecution::ExternalResolved { package } => match &package.implementation {
                        ExternalCheckPackageImplementation::Declarative(d) => {
                            Some((d.clone(), run.config.clone(), exclusion_matcher))
                        }
                        _ => None,
                    },
                    _ => None,
                });
        }

        // Build the fix schedule: connected components of the conflict graph
        // become groups whose checks must run serially; groups themselves are
        // file-disjoint and may run concurrently.
        let groups = crate::fix::scheduler::build_fix_schedule(fix_plan);

        // Ensure every check in fix_plan appears in the output map so callers
        // can distinguish "no fix available" (empty Vec) from "not scheduled".
        let mut accumulated: BTreeMap<String, Vec<crate::external::FixInvocationOutcome>> =
            fix_plan.keys().map(|k| (k.clone(), Vec::new())).collect();

        // Register all checks with the reporter up front so the apply-phase
        // status lines appear before any invocations begin.
        for (check_id, files) in fix_plan {
            reporter.register(check_id, files.len());
        }

        let source_tree = Arc::clone(&self.source_tree);
        let max = max_passes.max(1);

        for pass in 0..max {
            // Run groups concurrently. Groups are file-disjoint so copy-backs
            // in different groups cannot race on any real file.
            let pass_results: Vec<Vec<(String, Vec<crate::external::FixInvocationOutcome>)>> = groups
                .par_iter()
                .map(|group| {
                    let source_tree = Arc::clone(&source_tree);
                    let reporter = Arc::clone(&reporter);
                    let mut group_results: Vec<(String, Vec<crate::external::FixInvocationOutcome>)> = Vec::new();

                    // Apply checks serially in category order so each check
                    // sandboxes the latest bytes written by its predecessor.
                    for check_id in &group.ordered_checks {
                        let fix_start = Instant::now();
                        reporter.start_fix(check_id, pass + 1);
                        let reporter_clone = Arc::clone(&reporter);
                        let check_id_for_progress = check_id.clone();
                        let invocation_outcomes = match declarative_info.get(check_id) {
                            Some(Some((package, config, exclusion_matcher))) => crate::external::run_declarative_fix(
                                repo_root,
                                package,
                                &fix_plan[check_id],
                                source_tree.as_ref(),
                                config,
                                exclusion_matcher,
                                move |n| reporter_clone.record_progress(&check_id_for_progress, n),
                            ),
                            _ => Vec::new(), // non-declarative or missing → no fix
                        };
                        let error_count: usize = invocation_outcomes
                            .iter()
                            .map(|inv| inv.per_file_errors.len() + if inv.error.is_some() { 1 } else { 0 })
                            .sum();
                        reporter.finish(check_id, error_count, fix_start.elapsed());
                        group_results.push((check_id.clone(), invocation_outcomes));
                    }

                    group_results
                })
                .collect();

            // Merge this pass into accumulated outcomes; check for convergence.
            let mut any_applied = false;
            for group_results in pass_results {
                for (check_id, inv_outcomes) in group_results {
                    if inv_outcomes.iter().any(|inv| !inv.applied.is_empty()) {
                        any_applied = true;
                    }
                    accumulated.entry(check_id).or_default().extend(inv_outcomes);
                }
            }

            // A pass that wrote no files means the pipeline has converged.
            if !any_applied {
                break;
            }
        }

        Ok(accumulated)
    }

    pub fn run_starlark_fixes(
        &self,
        changeset: &ChangeSet,
        results: &[CheckResult],
        fix_plan: &BTreeMap<String, Vec<PathBuf>>,
        repo_root: &Path,
    ) -> Result<BTreeMap<String, Vec<crate::external::FixInvocationOutcome>>> {
        use crate::external::FixInvocationOutcome;
        use crate::external::sandbox::HostCeiling;
        use crate::fix::WritableSandbox;

        let scheduled = self.schedule_runs(changeset)?;
        let findings_by_check: BTreeMap<&str, Vec<Finding>> = results
            .iter()
            .map(|result| (result.check_id.as_str(), result.findings.clone()))
            .collect();
        let ceiling = HostCeiling::new(repo_root);
        let mut outcomes: BTreeMap<String, Vec<FixInvocationOutcome>> = BTreeMap::new();

        for run in scheduled.runs {
            let check_id = run.configured_check_id.clone();
            let Some(fixable_files) = fix_plan.get(&check_id) else {
                continue;
            };
            let ScheduledExecution::StarlarkLocal {
                check,
                output,
                package_tree,
                fix_path: Some(fix_path),
                checkleft_root,
                check_dir,
            } = run.execution
            else {
                continue;
            };
            let fix_source = match package_tree.read_file(&fix_path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(source) => StarlarkCheckSource::file(check_id.clone(), fix_path.clone(), source)
                        .with_load_context(checkleft_root, check_dir),
                    Err(err) => {
                        outcomes.insert(
                            check_id,
                            vec![FixInvocationOutcome {
                                invocation_id: "starlark_fix".to_owned(),
                                applied: Vec::new(),
                                per_file_errors: Vec::new(),
                                error: Some(anyhow!("{} is not valid UTF-8: {err}", fix_path.display())),
                            }],
                        );
                        continue;
                    }
                },
                Err(err) => {
                    outcomes.insert(
                        check_id,
                        vec![FixInvocationOutcome {
                            invocation_id: "starlark_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(anyhow!("failed to read {}: {err:#}", fix_path.display())),
                        }],
                    );
                    continue;
                }
            };
            let findings = findings_by_check.get(check_id.as_str()).cloned().unwrap_or_default();
            let edits = match check.evaluate_fix_prepared_adapter(
                fix_source,
                output.as_ref(),
                &findings,
                package_tree.as_ref(),
            ) {
                Ok(edits) => edits,
                Err(err) => {
                    outcomes.insert(
                        check_id,
                        vec![FixInvocationOutcome {
                            invocation_id: "starlark_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(err),
                        }],
                    );
                    continue;
                }
            };
            let fixable_set: HashSet<&PathBuf> = fixable_files.iter().collect();
            let mut edits_by_file: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
            for edit in edits {
                if fixable_set.contains(&edit.path) && !run.exclusion_matcher.is_excluded(&edit.path) {
                    edits_by_file
                        .entry(edit.path)
                        .or_default()
                        .push((edit.old_text, edit.new_text));
                }
            }
            if edits_by_file.is_empty() {
                continue;
            }

            let files_to_stage = edits_by_file.keys().cloned().collect::<Vec<_>>();
            let sandbox = match WritableSandbox::stage(&files_to_stage, self.source_tree.as_ref(), &ceiling) {
                Ok(sandbox) => sandbox,
                Err(err) => {
                    outcomes.insert(
                        check_id,
                        vec![FixInvocationOutcome {
                            invocation_id: "starlark_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(err),
                        }],
                    );
                    continue;
                }
            };

            let mut apply_err = None;
            'files: for (path, edits) in &edits_by_file {
                let staged = sandbox.root_path().join(path);
                if !staged.exists() {
                    continue;
                }
                let content = match std::fs::read_to_string(&staged) {
                    Ok(content) => content,
                    Err(err) => {
                        apply_err = Some(anyhow!("failed to read staged file {}: {err}", path.display()));
                        break 'files;
                    }
                };
                let mut new_content = content;
                for (old_text, new_text) in edits {
                    new_content = new_content.replacen(old_text.as_str(), new_text.as_str(), 1);
                }
                if let Err(err) = std::fs::write(&staged, new_content.as_bytes()) {
                    apply_err = Some(anyhow!(
                        "failed to write edited file to sandbox {}: {err}",
                        path.display()
                    ));
                    break 'files;
                }
            }

            if let Some(err) = apply_err {
                outcomes.insert(
                    check_id,
                    vec![FixInvocationOutcome {
                        invocation_id: "starlark_fix".to_owned(),
                        applied: Vec::new(),
                        per_file_errors: Vec::new(),
                        error: Some(err),
                    }],
                );
                continue;
            }

            let changed = match sandbox.detect_changes() {
                Ok(changed) => changed,
                Err(err) => {
                    outcomes.insert(
                        check_id,
                        vec![FixInvocationOutcome {
                            invocation_id: "starlark_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(err),
                        }],
                    );
                    continue;
                }
            };
            let report = sandbox.copy_back(&changed, repo_root);
            let error = report.failed.map(|(_, err)| err);
            outcomes.insert(
                check_id,
                vec![FixInvocationOutcome {
                    invocation_id: "starlark_fix".to_owned(),
                    applied: report.applied,
                    per_file_errors: Vec::new(),
                    error,
                }],
            );
        }

        Ok(outcomes)
    }

    /// Apply `suggested_fix` edits from built-in check findings through the T2
    /// writable copy sandbox + atomic copy-back path.
    ///
    /// For each check in `fix_plan`, collects [`crate::output::FileEdit`]s from
    /// `Error`- and `Warning`-severity findings that carry a `suggested_fix`. Each
    /// edit whose `path` is in the check's fixable set is applied as a text
    /// substitution (first occurrence of `old_text` → `new_text`) inside a staged
    /// writable sandbox. Only files that actually changed are atomically copied back
    /// to the real tree.
    ///
    /// The returned map uses the same `check_id → Vec<FixInvocationOutcome>`
    /// shape as [`Self::run_declarative_fixes`]. An absent entry (or empty `Vec`)
    /// means no `suggested_fix` edits were found for that check; the caller merges
    /// this with the declarative outcomes and treats empty as "no fix available".
    pub fn apply_suggested_fixes(
        &self,
        results: &[CheckResult],
        fix_plan: &BTreeMap<String, Vec<PathBuf>>,
        repo_root: &Path,
    ) -> BTreeMap<String, Vec<crate::external::FixInvocationOutcome>> {
        use crate::external::FixInvocationOutcome;
        use crate::external::sandbox::HostCeiling;
        use crate::fix::WritableSandbox;

        let mut outcomes: BTreeMap<String, Vec<FixInvocationOutcome>> = BTreeMap::new();
        let ceiling = HostCeiling::new(repo_root);

        for result in results {
            let check_id = &result.check_id;
            let Some(fixable_files) = fix_plan.get(check_id) else {
                continue;
            };
            let fixable_set: HashSet<&PathBuf> = fixable_files.iter().collect();

            // Collect FileEdits from actionable findings, restricted to the fixable
            // set. Multiple findings may contribute edits for the same file; they are
            // applied in document order.
            let mut edits_by_file: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
            for finding in &result.findings {
                if !matches!(finding.severity, Severity::Error | Severity::Warning) {
                    continue;
                }
                let Some(sf) = &finding.suggested_fix else {
                    continue;
                };
                for edit in &sf.edits {
                    if fixable_set.contains(&edit.path) {
                        edits_by_file
                            .entry(edit.path.clone())
                            .or_default()
                            .push((edit.old_text.clone(), edit.new_text.clone()));
                    }
                }
            }

            if edits_by_file.is_empty() {
                // No suggested_fix edits for this check: leave the entry absent so
                // the caller falls through to "no fix available".
                continue;
            }

            let files_to_stage: Vec<PathBuf> = edits_by_file.keys().cloned().collect();

            // Stage a writable, force-copied sandbox (never hardlinked — a hardlink
            // would escape copy-back; see safety.rs module doc).
            let sandbox = match WritableSandbox::stage(&files_to_stage, self.source_tree.as_ref(), &ceiling) {
                Ok(s) => s,
                Err(err) => {
                    outcomes.insert(
                        check_id.clone(),
                        vec![FixInvocationOutcome {
                            invocation_id: "suggested_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(err),
                        }],
                    );
                    continue;
                }
            };

            // Apply each edit to the staged sandbox copy as a text substitution.
            // Files absent from the source tree were silently dropped by staging and
            // are skipped here. If old_text is absent (file already fixed by a prior
            // pass or the edit doesn't apply), the content is unchanged and
            // detect_changes will produce no copy-back for that file.
            let mut apply_err: Option<anyhow::Error> = None;
            'files: for (path, edits) in &edits_by_file {
                let staged = sandbox.root_path().join(path);
                if !staged.exists() {
                    continue; // absent from source tree; dropped by staging
                }
                let content = match std::fs::read_to_string(&staged) {
                    Ok(c) => c,
                    Err(err) => {
                        apply_err = Some(anyhow!("failed to read staged file {}: {err}", path.display()));
                        break 'files;
                    }
                };
                let mut new_content = content;
                for (old_text, new_text) in edits {
                    new_content = new_content.replacen(old_text.as_str(), new_text.as_str(), 1);
                }
                if let Err(err) = std::fs::write(&staged, new_content.as_bytes()) {
                    apply_err = Some(anyhow!(
                        "failed to write edited file to sandbox {}: {err}",
                        path.display()
                    ));
                    break 'files;
                }
            }

            if let Some(err) = apply_err {
                outcomes.insert(
                    check_id.clone(),
                    vec![FixInvocationOutcome {
                        invocation_id: "suggested_fix".to_owned(),
                        applied: Vec::new(),
                        per_file_errors: Vec::new(),
                        error: Some(err),
                    }],
                );
                continue;
            }

            // Detect which staged files actually changed, then copy them back.
            let changed = match sandbox.detect_changes() {
                Ok(c) => c,
                Err(err) => {
                    outcomes.insert(
                        check_id.clone(),
                        vec![FixInvocationOutcome {
                            invocation_id: "suggested_fix".to_owned(),
                            applied: Vec::new(),
                            per_file_errors: Vec::new(),
                            error: Some(err),
                        }],
                    );
                    continue;
                }
            };

            let report = sandbox.copy_back(&changed, repo_root);
            let error = report.failed.map(|(_, e)| e);
            outcomes.insert(
                check_id.clone(),
                vec![FixInvocationOutcome {
                    invocation_id: "suggested_fix".to_owned(),
                    applied: report.applied,
                    per_file_errors: Vec::new(),
                    error,
                }],
            );
        }

        outcomes
    }

    pub fn list_configured_checks(&self, changeset: &ChangeSet) -> Result<Vec<String>> {
        info!(
            changed_files = changeset.changed_files.len(),
            "listing configured checks"
        );
        let mut checks = BTreeSet::new();
        let mut resolution_errors = BTreeMap::new();
        let mut config_diagnostics = BTreeSet::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            for diagnostic in resolved.diagnostics() {
                config_diagnostics.insert(format_config_diagnostic(diagnostic));
            }
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            for check in resolved.enabled() {
                if let ScheduledExecution::Invalid { message, .. } = self.resolve_scheduled_execution(check) {
                    resolution_errors.insert(check.id.clone(), message);
                }
                checks.insert(check.id.clone());
            }
        }

        let mut starlark_diagnostics = BTreeMap::new();
        if let Ok(discovered) = self.discover_starlark_checks(changeset, &mut starlark_diagnostics) {
            let adapters = AdapterRegistry::with_builtin_adapters();
            for (check, _) in discovered {
                let Ok(adapter) = adapters.require(&check.adapter) else {
                    continue;
                };
                if starlark_changeset_for_check(changeset, &check, adapter.as_ref())
                    .map(|changeset| !changeset.changed_files.is_empty())
                    .unwrap_or(false)
                {
                    checks.insert(check.id);
                }
            }
        }
        for diagnostic in starlark_diagnostics.values() {
            for finding in &diagnostic.findings {
                config_diagnostics.insert(format!(
                    "`{}` at {}: {}",
                    diagnostic.check_id,
                    finding
                        .location
                        .as_ref()
                        .map(|location| location.path.display().to_string())
                        .unwrap_or_else(|| "<unknown>".to_owned()),
                    finding.message
                ));
            }
        }

        if !resolution_errors.is_empty() {
            let details = resolution_errors
                .into_iter()
                .map(|(check_id, message)| format!("`{check_id}`: {message}"))
                .collect::<Vec<_>>()
                .join("\n- ");
            bail!("failed to resolve external check packages:\n- {details}");
        }

        if !config_diagnostics.is_empty() {
            let details = config_diagnostics.into_iter().collect::<Vec<_>>().join("\n- ");
            bail!("failed to resolve checks configuration:\n- {details}");
        }

        Ok(checks.into_iter().collect())
    }

    fn schedule_runs(&self, changeset: &ChangeSet) -> Result<ScheduledRuns> {
        info!(changed_files = changeset.changed_files.len(), "scheduling checks");
        let mut grouped_runs: BTreeMap<(String, String, String, String, String, String), ScheduledCheckRun> =
            BTreeMap::new();
        // Dedup key for config diagnostics:
        // (check_id, path, line, column, message, remediation).
        type DiagnosticGroupKey = (String, PathBuf, Option<u32>, Option<u32>, String, String);
        let mut grouped_diagnostics: BTreeMap<DiagnosticGroupKey, CheckResult> = BTreeMap::new();
        let mut starlark_changeset = ChangeSet {
            changed_files: Vec::new(),
            file_line_deltas: HashMap::new(),
            file_diffs: HashMap::new(),
            commit_description: changeset.commit_description.clone(),
            pr_description: changeset.pr_description.clone(),
            change_id: changeset.change_id.clone(),
            repository: changeset.repository.clone(),
        };

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            info!(path = %changed_file.path.display(), "evaluating changed file");

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            for diagnostic in resolved.diagnostics() {
                let remediation = diagnostic.remediation.clone().unwrap_or_default();
                let key = (
                    diagnostic.check_id.clone(),
                    diagnostic.location.path.clone(),
                    diagnostic.location.line,
                    diagnostic.location.column,
                    diagnostic.message.clone(),
                    remediation,
                );
                grouped_diagnostics
                    .entry(key)
                    .or_insert_with(|| config_diagnostic_result(diagnostic));
            }
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            if !global_excludes_file(changed_file, &resolved) {
                starlark_changeset.changed_files.push(ChangedFile {
                    path: changed_file.path.clone(),
                    kind: changed_file.kind,
                    old_path: changed_file.old_path.clone(),
                });
                if let Some(delta) = changeset.file_line_deltas.get(&changed_file.path) {
                    starlark_changeset
                        .file_line_deltas
                        .insert(changed_file.path.clone(), *delta);
                }
                if let Some(diff) = changeset.file_diffs.get(&changed_file.path) {
                    starlark_changeset
                        .file_diffs
                        .insert(changed_file.path.clone(), diff.clone());
                }
            }
            for check in resolved.enabled() {
                if self.is_explicit_starlark_selection(&resolved, check) {
                    if !toml_value_is_empty_table(&check.config) {
                        let diagnostic = ConfigDiagnostic {
                            check_id: check.id.clone(),
                            message: "`config` is not supported on CHECKS.yaml entries that select Starlark package checks".to_owned(),
                            location: Location {
                                path: check.source_path.clone(),
                                line: None,
                                column: None,
                            },
                            remediation: Some(
                                "Move check behavior into check.checkleft/check_meta and publish a new package version."
                                    .to_owned(),
                            ),
                        };
                        let key = (
                            diagnostic.check_id.clone(),
                            diagnostic.location.path.clone(),
                            diagnostic.location.line,
                            diagnostic.location.column,
                            diagnostic.message.clone(),
                            diagnostic.remediation.clone().unwrap_or_default(),
                        );
                        grouped_diagnostics
                            .entry(key)
                            .or_insert_with(|| config_diagnostic_result(&diagnostic));
                    }
                    continue;
                }
                let policy = self.resolve_effective_policy(check);
                let config_fingerprint = toml::to_string(&check.config).unwrap_or_default();
                let implementation_fingerprint = check
                    .implementation
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let policy_fingerprint = policy.fingerprint();
                // The effective exclude set (global ∪ per-check) can differ between two
                // files that resolve to the same check id (a subdirectory `CHECKS` may
                // add a global exclude). Fold it into the grouping key so files with a
                // different effective matcher never share a run — each run then carries
                // one consistent matcher.
                let effective_exclude_patterns: Vec<String> = resolved
                    .global_exclude_patterns()
                    .iter()
                    .chain(check.exclude_patterns.iter())
                    .cloned()
                    .collect();
                let exclude_fingerprint = effective_exclude_patterns.join("\n");
                let key = (
                    check.id.clone(),
                    check.check.clone(),
                    implementation_fingerprint,
                    config_fingerprint,
                    policy_fingerprint,
                    exclude_fingerprint,
                );

                let entry = grouped_runs.entry(key).or_insert_with(|| {
                    let execution = self.resolve_scheduled_execution(check);
                    let mut effective_policy = policy;
                    if let ScheduledExecution::ExternalResolved { package } = &execution
                        && let ExternalCheckPackageImplementation::Declarative(d) = &package.implementation
                        && d.invocations.iter().any(|inv| inv.transform.uses_dynamic_severity())
                    {
                        effective_policy.preserve_finding_severity = true;
                    }
                    // An invalid exclude glob fails matcher construction; fall back to a
                    // no-op matcher (excludes nothing) so a malformed pattern never
                    // crashes the whole run — config validation surfaces it elsewhere.
                    let exclusion_matcher = ExclusionMatcher::new(&effective_exclude_patterns).unwrap_or_else(|err| {
                        tracing::warn!(check_id = %check.id, error = %err, "invalid exclude glob; treating as no exclusions");
                        ExclusionMatcher::default()
                    });
                    ScheduledCheckRun {
                        configured_check_id: check.id.clone(),
                        source_path: check.source_path.clone(),
                        execution,
                        policy: effective_policy,
                        config: check.config.clone(),
                        changeset: ChangeSet {
                            changed_files: Vec::new(),
                            file_line_deltas: HashMap::new(),
                            file_diffs: HashMap::new(),
                            commit_description: changeset.commit_description.clone(),
                            pr_description: changeset.pr_description.clone(),
                            change_id: changeset.change_id.clone(),
                            repository: changeset.repository.clone(),
                        },
                        exclusion_matcher,
                    }
                });

                let already_present = entry
                    .changeset
                    .changed_files
                    .iter()
                    .any(|scheduled_file| scheduled_file.path == changed_file.path);
                if !already_present {
                    entry.changeset.changed_files.push(ChangedFile {
                        path: changed_file.path.clone(),
                        kind: changed_file.kind,
                        old_path: changed_file.old_path.clone(),
                    });
                    if let Some(delta) = changeset.file_line_deltas.get(&changed_file.path) {
                        entry
                            .changeset
                            .file_line_deltas
                            .insert(changed_file.path.clone(), *delta);
                    }
                    if let Some(diff) = changeset.file_diffs.get(&changed_file.path) {
                        entry
                            .changeset
                            .file_diffs
                            .insert(changed_file.path.clone(), diff.clone());
                    }
                }
            }
        }
        self.schedule_local_starlark_runs(&starlark_changeset, &mut grouped_runs, &mut grouped_diagnostics);

        Ok(ScheduledRuns {
            runs: grouped_runs.into_values().collect(),
            diagnostics: grouped_diagnostics.into_values().collect(),
        })
    }

    fn schedule_local_starlark_runs(
        &self,
        changeset: &ChangeSet,
        grouped_runs: &mut BTreeMap<(String, String, String, String, String, String), ScheduledCheckRun>,
        grouped_diagnostics: &mut BTreeMap<(String, PathBuf, Option<u32>, Option<u32>, String, String), CheckResult>,
    ) {
        let discovered = match self.discover_starlark_checks(changeset, grouped_diagnostics) {
            Ok(checks) => checks,
            Err(err) => {
                let diagnostic = ConfigDiagnostic {
                    check_id: "starlark-discovery".to_owned(),
                    message: format!("failed to discover local Starlark checks: {err:#}"),
                    location: Location {
                        path: PathBuf::from("checkleft/package.toml"),
                        line: None,
                        column: None,
                    },
                    remediation: Some("Fix the local checkleft package structure or package.toml.".to_owned()),
                };
                let key = (
                    diagnostic.check_id.clone(),
                    diagnostic.location.path.clone(),
                    diagnostic.location.line,
                    diagnostic.location.column,
                    diagnostic.message.clone(),
                    diagnostic.remediation.clone().unwrap_or_default(),
                );
                grouped_diagnostics
                    .entry(key)
                    .or_insert_with(|| config_diagnostic_result(&diagnostic));
                return;
            }
        };

        let adapters = AdapterRegistry::with_builtin_adapters();
        let mut adapter_outputs: BTreeMap<String, Arc<AdapterPreparedOutput>> = BTreeMap::new();
        for (check, package_tree) in discovered {
            let adapter = match adapters.require(&check.adapter) {
                Ok(adapter) => adapter,
                Err(err) => {
                    self.insert_starlark_invalid_run(grouped_runs, &check, err.to_string());
                    continue;
                }
            };
            let check_changeset = match starlark_changeset_for_check(changeset, &check, adapter.as_ref()) {
                Ok(changeset) => changeset,
                Err(err) => {
                    let diagnostic = ConfigDiagnostic {
                        check_id: check.id.clone(),
                        message: format!("failed to schedule local Starlark check: {err:#}"),
                        location: Location {
                            path: check.check_path.clone(),
                            line: None,
                            column: None,
                        },
                        remediation: Some("Fix check_meta(applies_to = [...]) for this check.".to_owned()),
                    };
                    let key = (
                        diagnostic.check_id.clone(),
                        diagnostic.location.path.clone(),
                        diagnostic.location.line,
                        diagnostic.location.column,
                        diagnostic.message.clone(),
                        diagnostic.remediation.clone().unwrap_or_default(),
                    );
                    grouped_diagnostics
                        .entry(key)
                        .or_insert_with(|| config_diagnostic_result(&diagnostic));
                    continue;
                }
            };
            if check_changeset.changed_files.is_empty() {
                continue;
            }
            let output_key = starlark_adapter_output_key(&check.adapter, &check_changeset);
            let adapter_output = match adapter_outputs.get(&output_key) {
                Some(output) => Arc::clone(output),
                None => {
                    let output = match adapter.prepare(AdapterInput {
                        changeset: &check_changeset,
                        tree: self.source_tree.as_ref(),
                        applies_to: &check.check_meta.applies_to,
                        package_scope: Some(&check.scope_root),
                    }) {
                        Ok(output) => Arc::new(output),
                        Err(err) => {
                            self.insert_starlark_invalid_run(
                                grouped_runs,
                                &check,
                                format!("failed to prepare Starlark adapter `{}`: {err:#}", check.adapter),
                            );
                            continue;
                        }
                    };
                    adapter_outputs.insert(output_key, Arc::clone(&output));
                    output
                }
            };

            let source = match package_tree.read_file(&check.check_path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(source) => source,
                    Err(err) => {
                        self.insert_starlark_invalid_run(
                            grouped_runs,
                            &check,
                            format!("{} is not valid UTF-8: {err}", check.check_path.display()),
                        );
                        continue;
                    }
                },
                Err(err) => {
                    self.insert_starlark_invalid_run(
                        grouped_runs,
                        &check,
                        format!("failed to read {}: {err:#}", check.check_path.display()),
                    );
                    continue;
                }
            };
            let runner = Arc::new(StarlarkCheckRunner::new(
                StarlarkCheckSource::file(check.id.clone(), check.check_path.clone(), source)
                    .with_load_context(check.checkleft_root.clone(), check.check_dir.clone()),
            ));
            let key = (
                check.id.clone(),
                "starlark-local".to_owned(),
                check.check_path.display().to_string(),
                String::new(),
                starlark_policy(&check.id).fingerprint(),
                String::new(),
            );
            grouped_runs.insert(
                key,
                ScheduledCheckRun {
                    configured_check_id: check.id.clone(),
                    source_path: check.check_path.clone(),
                    execution: ScheduledExecution::StarlarkLocal {
                        check: runner,
                        output: adapter_output,
                        package_tree,
                        fix_path: check.fix_path.clone(),
                        checkleft_root: check.checkleft_root.clone(),
                        check_dir: check.check_dir.clone(),
                    },
                    policy: starlark_policy(&check.id),
                    config: toml::Value::Table(Default::default()),
                    changeset: check_changeset,
                    exclusion_matcher: ExclusionMatcher::default(),
                },
            );
        }
    }

    fn discover_starlark_checks(
        &self,
        changeset: &ChangeSet,
        grouped_diagnostics: &mut BTreeMap<(String, PathBuf, Option<u32>, Option<u32>, String, String), CheckResult>,
    ) -> Result<Vec<(DiscoveredCheck, Arc<dyn SourceTree>)>> {
        let package_selections = self.starlark_package_selections_for_changeset(changeset);
        if package_selections.is_empty() {
            let tree: Arc<dyn SourceTree> = Arc::clone(&self.source_tree);
            return Ok(discovery::discover_local_checks(changeset, self.source_tree.as_ref())?
                .into_iter()
                .map(|check| (check, Arc::clone(&tree)))
                .collect());
        }

        let mut checks = Vec::new();
        let mut selected_refs = BTreeMap::new();
        for selection in package_selections {
            match self.discover_starlark_package_checks(&selection.package, &mut selected_refs) {
                Ok(mut package_checks) => {
                    if selection.package.activation == StarlarkPackageActivation::Explicit {
                        package_checks
                            .retain(|(check, _)| explicit_selection_matches(&selection.explicit_check_ids, &check.id));
                    }
                    for (check, _) in &mut package_checks {
                        check.scope_root = selection.package.config_dir.clone();
                        check.activation_include_patterns = selection.package.include_patterns.clone();
                        check.activation_exclude_patterns = selection.package.exclude_patterns.clone();
                    }
                    checks.append(&mut package_checks);
                }
                Err(err) => self.insert_starlark_package_diagnostic(
                    grouped_diagnostics,
                    &selection.package,
                    format!(
                        "failed to activate Starlark package `{}`: {err:#}",
                        selection.package.source
                    ),
                ),
            }
        }
        checks.sort_by(|(left, _), (right, _)| {
            starlark_discovered_check_key(left).cmp(&starlark_discovered_check_key(right))
        });
        checks.dedup_by(|(left, _), (right, _)| {
            starlark_discovered_check_key(left) == starlark_discovered_check_key(right)
        });
        Ok(checks)
    }

    fn starlark_package_selections_for_changeset(&self, changeset: &ChangeSet) -> Vec<SelectedStarlarkPackage> {
        let mut packages: BTreeMap<String, SelectedStarlarkPackage> = BTreeMap::new();
        for changed_file in &changeset.changed_files {
            let Ok(resolved) = self.resolver.resolve_for_file(&changed_file.path) else {
                continue;
            };
            let explicit_ids = resolved
                .enabled()
                .map(|check| check.id.clone())
                .collect::<BTreeSet<_>>();
            for package in resolved.starlark_packages() {
                if !starlark_package_applies_to_path(package, &changed_file.path) {
                    continue;
                }
                packages
                    .entry(starlark_package_selection_key(package))
                    .and_modify(|selection| {
                        selection.explicit_check_ids.extend(explicit_ids.iter().cloned());
                        if package.activation == StarlarkPackageActivation::All {
                            selection.package.activation = StarlarkPackageActivation::All;
                        }
                    })
                    .or_insert_with(|| SelectedStarlarkPackage {
                        package: package.clone(),
                        explicit_check_ids: explicit_ids.clone(),
                    });
            }
        }
        packages.into_values().collect()
    }

    fn is_explicit_starlark_selection(&self, resolved: &crate::config::ResolvedChecks, check: &CheckConfig) -> bool {
        if check.implementation.is_some() || self.registry.get(&check.check).is_some() {
            return false;
        }
        resolved
            .starlark_packages()
            .iter()
            .any(|package| package.activation == StarlarkPackageActivation::Explicit)
    }

    fn discover_starlark_package_checks(
        &self,
        package: &StarlarkPackageConfig,
        selected_refs: &mut BTreeMap<String, SelectedPackageRef>,
    ) -> Result<Vec<(DiscoveredCheck, Arc<dyn SourceTree>)>> {
        let resolved =
            self.resolve_starlark_package_source(&package.source, &package.version, package.sha256.as_deref())?;
        let root = resolved.root.as_path();
        let tree = resolved.tree;

        match package.kind {
            crate::config::StarlarkPackageKind::Package => {
                let manifest = PackageManifest::read_from_tree(tree.as_ref(), root)?;
                if manifest.package.kind != PackageKind::CheckPackage {
                    bail!(
                        "{} declares a package activation but package.toml kind is not `check_package`",
                        root.display()
                    );
                }
                ensure_selected_version_matches(&manifest, &package.version, root)?;
                record_selected_package_ref(
                    selected_refs,
                    &manifest.package.name,
                    SelectedPackageRef {
                        source: package.source.clone(),
                        version: package.version.clone(),
                        sha256: package.sha256.clone(),
                    },
                )?;
                Ok(discovery::discover_package_checks(tree.as_ref(), root)?
                    .into_iter()
                    .map(|check| (check, Arc::clone(&tree)))
                    .collect())
            }
            crate::config::StarlarkPackageKind::VersionSet => {
                let manifest = PackageManifest::read_from_tree(tree.as_ref(), root)?;
                if manifest.package.kind != PackageKind::VersionSet {
                    bail!(
                        "{} declares a version-set activation but package.toml kind is not `version_set`",
                        root.display()
                    );
                }
                ensure_selected_version_matches(&manifest, &package.version, root)?;
                record_selected_package_ref(
                    selected_refs,
                    &manifest.package.name,
                    SelectedPackageRef {
                        source: package.source.clone(),
                        version: package.version.clone(),
                        sha256: package.sha256.clone(),
                    },
                )?;
                let mut checks = Vec::new();
                for (alias, include) in manifest.includes {
                    let include_resolved = self.resolve_starlark_package_source(
                        &include.source,
                        &include.version,
                        include.sha256.as_deref(),
                    )?;
                    let include_root = include_resolved.root.as_path();
                    let include_tree = include_resolved.tree;
                    let include_manifest = PackageManifest::read_from_tree(include_tree.as_ref(), include_root)?;
                    if include_manifest.package.kind != PackageKind::CheckPackage {
                        bail!(
                            "version-set include `{alias}` points at {}, but included package kind is not `check_package`",
                            include_root.display()
                        );
                    }
                    ensure_include_version_matches(&alias, &include_manifest, &include, include_root)?;
                    record_selected_package_ref(
                        selected_refs,
                        &include_manifest.package.name,
                        SelectedPackageRef {
                            source: include.source.clone(),
                            version: include.version.clone(),
                            sha256: include.sha256.clone(),
                        },
                    )?;
                    checks.extend(
                        discovery::discover_package_checks(include_tree.as_ref(), include_root)?
                            .into_iter()
                            .map(|check| (check, Arc::clone(&include_tree))),
                    );
                }
                Ok(checks)
            }
        }
    }

    fn resolve_starlark_package_source(
        &self,
        source: &str,
        version: &str,
        expected_sha256: Option<&str>,
    ) -> Result<ResolvedStarlarkPackage> {
        if let Some(git_source) = source.strip_prefix("git://") {
            let expected = expected_sha256.ok_or_else(|| anyhow!("git package refs must declare sha256"))?;
            let bytes = git_archive_checkleft(git_source, version)?;
            let actual = sha256_hex(&bytes);
            if actual != expected {
                bail!(
                    "Starlark package git archive {} at {} sha256 mismatch: expected {}, got {}",
                    git_source,
                    version,
                    expected,
                    actual
                );
            }
            let tree = Arc::new(ArchivePackageTree::from_tar_with_stripped_prefix(
                &bytes,
                Path::new("checkleft"),
            )?);
            return Ok(ResolvedStarlarkPackage {
                root: PathBuf::new(),
                tree,
            });
        }

        let Some(path) = source.strip_prefix("path://").map(Path::new) else {
            bail!("registry package sources are parsed but not schedulable yet");
        };
        if path.extension().and_then(|ext| ext.to_str()) == Some("gz")
            && path
                .file_stem()
                .and_then(|stem| Path::new(stem).extension())
                .and_then(|ext| ext.to_str())
                == Some("tar")
        {
            let bytes = self
                .source_tree
                .read_file(path)
                .with_context(|| format!("failed to read Starlark package archive {}", path.display()))?;
            if let Some(expected) = expected_sha256 {
                let actual = sha256_hex(&bytes);
                if actual != expected {
                    bail!(
                        "Starlark package archive {} sha256 mismatch: expected {}, got {}",
                        path.display(),
                        expected,
                        actual
                    );
                }
            }
            let tree = Arc::new(ArchivePackageTree::from_tar_gz(&bytes)?);
            return Ok(ResolvedStarlarkPackage {
                root: PathBuf::new(),
                tree,
            });
        }
        Ok(ResolvedStarlarkPackage {
            root: path.to_path_buf(),
            tree: Arc::clone(&self.source_tree),
        })
    }

    fn insert_starlark_package_diagnostic(
        &self,
        grouped_diagnostics: &mut BTreeMap<(String, PathBuf, Option<u32>, Option<u32>, String, String), CheckResult>,
        package: &StarlarkPackageConfig,
        message: String,
    ) {
        let diagnostic = ConfigDiagnostic {
            check_id: "starlark-package".to_owned(),
            message,
            location: Location {
                path: package.source_path.clone(),
                line: None,
                column: None,
            },
            remediation: Some("Fix this checkleft_packages entry in CHECKS.yaml.".to_owned()),
        };
        let key = (
            diagnostic.check_id.clone(),
            diagnostic.location.path.clone(),
            diagnostic.location.line,
            diagnostic.location.column,
            diagnostic.message.clone(),
            diagnostic.remediation.clone().unwrap_or_default(),
        );
        grouped_diagnostics
            .entry(key)
            .or_insert_with(|| config_diagnostic_result(&diagnostic));
    }

    fn insert_starlark_invalid_run(
        &self,
        grouped_runs: &mut BTreeMap<(String, String, String, String, String, String), ScheduledCheckRun>,
        check: &DiscoveredCheck,
        message: String,
    ) {
        let key = (
            check.id.clone(),
            "starlark-local-invalid".to_owned(),
            check.check_path.display().to_string(),
            String::new(),
            starlark_policy(&check.id).fingerprint(),
            String::new(),
        );
        grouped_runs.insert(
            key,
            ScheduledCheckRun {
                configured_check_id: check.id.clone(),
                source_path: check.check_path.clone(),
                execution: ScheduledExecution::Invalid {
                    message,
                    remediation: Some("Fix this local Starlark check file.".to_owned()),
                },
                policy: starlark_policy(&check.id),
                config: toml::Value::Table(Default::default()),
                changeset: ChangeSet::default(),
                exclusion_matcher: ExclusionMatcher::default(),
            },
        );
    }

    fn resolve_effective_policy(&self, check: &CheckConfig) -> EffectiveCheckPolicy {
        let severity_override = check.policy.severity;
        let allow_bypass = check.policy.allow_bypass.unwrap_or(false);
        let bypass_name = check
            .policy
            .bypass_name
            .clone()
            .unwrap_or_else(|| bypass_name_for_check_id(&check.id));

        EffectiveCheckPolicy {
            severity_override,
            allow_bypass,
            bypass_name,
            preserve_finding_severity: false,
        }
    }

    fn resolve_scheduled_execution(&self, check: &CheckConfig) -> ScheduledExecution {
        info!(check_id = %check.id, "resolving configured check execution");
        let Some(implementation_ref) = check.implementation.clone() else {
            let Some(built_in) = self.registry.get(&check.check) else {
                return ScheduledExecution::BuiltInMissing {
                    implementation_check_id: check.check.clone(),
                };
            };

            return match built_in.configure_scoped(&check.config, check.source_path.parent()) {
                Ok(configured) => ScheduledExecution::BuiltInConfigured { check: configured },
                Err(err) => ScheduledExecution::Invalid {
                    message: err.to_string(),
                    remediation: Some("Fix this check's `config` block in the CHECKS file.".to_owned()),
                },
            };
        };

        let package = match self.external_package_provider.resolve(&implementation_ref) {
            Ok(Some(package)) => package,
            Ok(None) => {
                return ScheduledExecution::Invalid {
                    message: format!(
                        "external implementation `{implementation_ref}` for check `{}` was not found in configured providers",
                        check.id
                    ),
                    remediation: Some(
                        "If this is a file implementation, ensure the manifest path exists. If this is generated, ensure the generated index is configured and includes the ID."
                            .to_owned(),
                    ),
                };
            }
            Err(err) => {
                return ScheduledExecution::Invalid {
                    message: format!(
                        "failed to resolve external implementation `{implementation_ref}` for check `{}`: {err:#}",
                        check.id
                    ),
                    remediation: None,
                };
            }
        };

        if package.id != check.check {
            return ScheduledExecution::Invalid {
                message: format!(
                    "external package id mismatch for check `{}`: expected `{}`, got `{}`",
                    check.id, check.check, package.id
                ),
                remediation: Some(
                    "Set `check = ...` to match the external package `id` or update the package manifest.".to_owned(),
                ),
            };
        }

        if check.origin == CheckConfigOrigin::ExternalUrl
            && matches!(
                &package.implementation,
                ExternalCheckPackageImplementation::Declarative(_)
            )
        {
            // The declarative runtime runs real binaries the framework selects;
            // letting a remote-fetched config drive that is the same trust level
            // as shipping a binary, so it is rejected from `external_checks_url`.
            return ScheduledExecution::Invalid {
                message: format!(
                    "external check `{}` from `settings.external_checks_url` cannot use runtime `{}`",
                    check.id, package.runtime
                ),
                remediation: Some(
                    "Move this check definition into the repository's checked-in CHECKS file or use a sandboxed runtime."
                        .to_owned(),
                ),
            };
        }

        ScheduledExecution::ExternalResolved {
            package: Box::new(package),
        }
    }
}

fn starlark_policy(check_id: &str) -> EffectiveCheckPolicy {
    EffectiveCheckPolicy {
        severity_override: None,
        allow_bypass: false,
        bypass_name: bypass_name_for_check_id(check_id),
        preserve_finding_severity: true,
    }
}

fn explicit_selection_matches(explicit_check_ids: &BTreeSet<String>, check_id: &str) -> bool {
    explicit_check_ids.contains(check_id)
        || explicit_check_ids
            .iter()
            .filter_map(|configured_id| configured_id.split_once(':').map(|(_, suffix)| suffix))
            .any(|suffix| suffix == check_id)
}

fn toml_value_is_empty_table(value: &toml::Value) -> bool {
    matches!(value, toml::Value::Table(table) if table.is_empty())
}

fn starlark_package_selection_key(package: &StarlarkPackageConfig) -> String {
    format!(
        "{:?}\0{}\0{}\0{}\0{}\0{}\0{}",
        package.kind,
        package.source,
        package.version,
        package.sha256.as_deref().unwrap_or(""),
        package.config_dir.display(),
        package.include_patterns.join("\0"),
        package.exclude_patterns.join("\0")
    )
}

fn starlark_discovered_check_key(check: &DiscoveredCheck) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        check.check_path.display(),
        check.scope_root.display(),
        check.id,
        check.activation_include_patterns.join("\0"),
        check.activation_exclude_patterns.join("\0")
    )
}

fn starlark_package_applies_to_path(package: &StarlarkPackageConfig, path: &Path) -> bool {
    activation_globs_match_path(&package.include_patterns, &package.exclude_patterns, path).unwrap_or(false)
}

fn record_selected_package_ref(
    selected_refs: &mut BTreeMap<String, SelectedPackageRef>,
    package_name: &str,
    selected_ref: SelectedPackageRef,
) -> Result<()> {
    if let Some(existing) = selected_refs.get(package_name) {
        if existing != &selected_ref {
            bail!(
                "selected package `{package_name}` resolves to conflicting refs: {}@{} and {}@{}",
                existing.source,
                existing.version,
                selected_ref.source,
                selected_ref.version
            );
        }
        return Ok(());
    }
    selected_refs.insert(package_name.to_owned(), selected_ref);
    Ok(())
}

fn ensure_selected_version_matches(manifest: &PackageManifest, selected_version: &str, root: &Path) -> Result<()> {
    if manifest.package.version != selected_version {
        bail!(
            "{} package.toml version `{}` does not match selected version `{}`",
            root.display(),
            manifest.package.version,
            selected_version
        );
    }
    Ok(())
}

fn ensure_include_version_matches(
    alias: &str,
    manifest: &PackageManifest,
    include: &PackageRef,
    include_root: &Path,
) -> Result<()> {
    if manifest.package.version != include.version {
        bail!(
            "version-set include `{alias}` points at {}, whose package.toml version `{}` does not match selected version `{}`",
            include_root.display(),
            manifest.package.version,
            include.version
        );
    }
    Ok(())
}

#[derive(Debug)]
struct ArchivePackageTree {
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl ArchivePackageTree {
    fn from_tar_gz(bytes: &[u8]) -> Result<Self> {
        let decoder = GzDecoder::new(bytes);
        Self::from_tar_reader(decoder, None)
    }

    fn from_tar_with_stripped_prefix(bytes: &[u8], prefix: &Path) -> Result<Self> {
        Self::from_tar_reader(Cursor::new(bytes), Some(prefix))
    }

    fn from_tar_reader<R: Read>(reader: R, strip_prefix: Option<&Path>) -> Result<Self> {
        let mut archive = Archive::new(reader);
        let mut files = BTreeMap::new();
        for entry in archive
            .entries()
            .context("failed to read Starlark package archive entries")?
        {
            let mut entry = entry.context("failed to read Starlark package archive entry")?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry
                .path()
                .context("failed to read Starlark package archive entry path")?
                .into_owned();
            let path = match strip_prefix {
                Some(prefix) => match path.strip_prefix(prefix) {
                    Ok(stripped) if !stripped.as_os_str().is_empty() => stripped.to_path_buf(),
                    _ => continue,
                },
                None => path,
            };
            validate_archive_path(&path)?;
            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .with_context(|| format!("failed to read Starlark package archive entry {}", path.display()))?;
            if files.insert(path.clone(), contents).is_some() {
                bail!("Starlark package archive contains duplicate file {}", path.display());
            }
        }
        if !files.contains_key(Path::new("package.toml")) {
            bail!("Starlark package archive must contain package.toml at the archive root");
        }
        Ok(Self { files })
    }
}

impl SourceTree for ArchivePackageTree {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        validate_archive_path(path)?;
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("Starlark package archive has no file {}", path.display()))
    }

    fn exists(&self, path: &Path) -> bool {
        validate_archive_path(path).is_ok()
            && (self.files.contains_key(path)
                || self
                    .files
                    .keys()
                    .any(|file| path.as_os_str().is_empty() || file.starts_with(path)))
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        validate_archive_path(path)?;
        if self.files.contains_key(path) {
            bail!("Starlark package archive path is not a directory: {}", path.display());
        }
        let mut entries = BTreeSet::new();
        for file in self.files.keys() {
            let relative = if path.as_os_str().is_empty() {
                file.as_path()
            } else {
                let Ok(relative) = file.strip_prefix(path) else {
                    continue;
                };
                relative
            };
            let Some(first) = relative.components().next() else {
                continue;
            };
            if let std::path::Component::Normal(part) = first {
                entries.insert(if path.as_os_str().is_empty() {
                    PathBuf::from(part)
                } else {
                    path.join(part)
                });
            }
        }
        if entries.is_empty() && !self.exists(path) {
            bail!("Starlark package archive has no directory {}", path.display());
        }
        Ok(entries.into_iter().collect())
    }

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let glob = Glob::new(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?;
        let matcher = glob.compile_matcher();
        Ok(self
            .files
            .keys()
            .filter(|path| matcher.is_match(path))
            .cloned()
            .collect())
    }
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("Starlark package archive paths must be relative: {}", path.display());
    }
    crate::path::validate_relative_path(path)
        .with_context(|| format!("invalid Starlark package archive path {}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn git_archive_checkleft(git_source: &str, version: &str) -> Result<Vec<u8>> {
    if git_source.trim().is_empty() {
        bail!("git package source must not be empty");
    }
    if version.trim().is_empty() {
        bail!("git package version must not be empty");
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(git_source)
        .args(["archive", "--format=tar", version, "checkleft"])
        .output()
        .with_context(|| format!("failed to run git archive for {git_source} at {version}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "failed to archive git package {} at {}: {}",
            git_source,
            version,
            stderr.trim()
        );
    }
    Ok(output.stdout)
}

fn starlark_changeset_for_check(
    changeset: &ChangeSet,
    check: &DiscoveredCheck,
    adapter: &dyn FormatAdapter,
) -> Result<ChangeSet> {
    let applies_to = build_glob_set(&check.check_meta.applies_to)?;
    let activation_include = build_glob_set(&check.activation_include_patterns)?;
    let activation_exclude = build_glob_set(&check.activation_exclude_patterns)?;
    let package_scope = &check.scope_root;
    let changed_files = changeset
        .changed_files
        .iter()
        .filter(|changed| !matches!(changed.kind, ChangeKind::Deleted))
        .filter(|changed| path_in_scope(&changed.path, &package_scope))
        .filter(|changed| activation_glob_sets_match_path(&activation_include, &activation_exclude, &changed.path))
        .filter(|changed| {
            crate::starlark::adapter::adapter_matches_changed_file(adapter, &changed.path, changed.old_path.as_deref())
        })
        .filter(|changed| starlark_applies_to_path(&applies_to, &changed.path, &package_scope))
        .cloned()
        .collect::<Vec<_>>();

    let mut scoped = ChangeSet {
        changed_files,
        file_line_deltas: HashMap::new(),
        file_diffs: HashMap::new(),
        commit_description: changeset.commit_description.clone(),
        pr_description: changeset.pr_description.clone(),
        change_id: changeset.change_id.clone(),
        repository: changeset.repository.clone(),
    };
    for changed in &scoped.changed_files {
        if let Some(delta) = changeset.file_line_deltas.get(&changed.path) {
            scoped.file_line_deltas.insert(changed.path.clone(), *delta);
        }
        if let Some(diff) = changeset.file_diffs.get(&changed.path) {
            scoped.file_diffs.insert(changed.path.clone(), diff.clone());
        }
    }
    Ok(scoped)
}

fn starlark_adapter_output_key(adapter: &str, changeset: &ChangeSet) -> String {
    let mut parts = vec![format!("adapter={adapter}")];
    for changed in &changeset.changed_files {
        parts.push(format!(
            "{}:{}:{}",
            changed.path.display(),
            change_kind_name(changed.kind),
            changed
                .old_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        ));
    }
    parts.join("\n")
}

fn change_kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

fn activation_globs_match_path(include_patterns: &[String], exclude_patterns: &[String], path: &Path) -> Result<bool> {
    let include = build_glob_set(include_patterns)?;
    let exclude = build_glob_set(exclude_patterns)?;
    Ok(activation_glob_sets_match_path(&include, &exclude, path))
}

fn activation_glob_sets_match_path(include: &GlobSet, exclude: &GlobSet, path: &Path) -> bool {
    include.is_match(path) && !exclude.is_match(path)
}

fn path_in_scope(path: &Path, scope: &Path) -> bool {
    scope.as_os_str().is_empty() || path.starts_with(scope)
}

fn starlark_applies_to_path(applies_to: &GlobSet, path: &Path, scope: &Path) -> bool {
    if applies_to.is_match(path) {
        return true;
    }
    if scope.as_os_str().is_empty() {
        return false;
    }
    path.strip_prefix(scope)
        .map(|relative| applies_to.is_match(relative))
        .unwrap_or(false)
}

fn should_skip_file(changed_file: &ChangedFile, resolved: &crate::config::ResolvedChecks) -> bool {
    is_checks_config_file(&changed_file.path) && !resolved.include_config_files()
}

fn global_excludes_file(changed_file: &ChangedFile, resolved: &crate::config::ResolvedChecks) -> bool {
    ExclusionMatcher::new(resolved.global_exclude_patterns())
        .map(|matcher| matcher.is_excluded(&changed_file.path))
        .unwrap_or(false)
}

fn config_diagnostic_result(diagnostic: &ConfigDiagnostic) -> CheckResult {
    CheckResult {
        check_id: diagnostic.check_id.clone(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: diagnostic.message.clone(),
            location: Some(diagnostic.location.clone()),
            remediations: diagnostic.remediation.iter().cloned().collect(),
            suggested_fix: None,
        }],
    }
}

fn format_config_diagnostic(diagnostic: &ConfigDiagnostic) -> String {
    format!(
        "`{}` at {}: {}",
        diagnostic.check_id,
        diagnostic.location.path.display(),
        diagnostic.message
    )
}

/// Return the 1-based line number of the first line containing `entry`, or `None`.
/// Used to anchor a stale-exclusion finding on the exact `CHECKS` file line that holds
/// the dead entry (e.g. `"engine/src/app.rs::ServerState"`).
fn locate_entry_line(contents: &str, entry: &str) -> Option<u32> {
    contents
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(entry))
        .map(|(index, _)| (index + 1) as u32)
}

fn is_checks_config_file(path: &std::path::Path) -> bool {
    let file_name = path.file_name();
    file_name == Some(OsStr::new("CHECKS.yaml")) || file_name == Some(OsStr::new("CHECKS.toml"))
}

/// Drop findings located on files outside the run's change scope.
///
/// Tools may over-report relative to the changed-file set — clippy diagnoses a
/// whole crate when one of its files changed, rustfmt recurses into module
/// children — so the framework guarantees scope here rather than per check: a
/// finding survives only if it has no location (check-level errors) or its path
/// is one of the run's changed files. In `--all` mode the changeset *is* every
/// file in the repo (`all_files_changeset`), so this is naturally a no-op there.
fn scope_findings_to_changeset(result: &mut CheckResult, changeset: &ChangeSet) {
    let changed: HashSet<&Path> = changeset.changed_files.iter().map(|file| file.path.as_path()).collect();
    result.findings.retain(|finding| match &finding.location {
        None => true,
        Some(location) => changed.contains(location.path.as_path()),
    });
}

/// Uniform exclusion backstop: drop any finding whose `location.path` is excluded
/// for this check instance.
///
/// Selection-time subtraction already keeps excluded paths out of what a check
/// sees, but this post-filter makes the "no findings on an excluded path"
/// guarantee uniform across *every* check kind — including a check that ignores the
/// filtered changeset or derives a path some other way. Location-less findings
/// (check-level errors) are kept. Framework-meta findings that intentionally land
/// on an unchanged `CHECKS` file (config diagnostics, bypass-applied notices,
/// stale-exclusion findings) never flow through here — they are produced outside
/// the per-check result path — so they are exempt by construction.
fn drop_excluded_findings(result: &mut CheckResult, exclusion: &ExclusionMatcher) {
    result.findings.retain(|finding| match &finding.location {
        None => true,
        Some(location) => !exclusion.is_excluded(location.path.as_path()),
    });
}

fn apply_policy_to_result(
    mut result: CheckResult,
    policy: &EffectiveCheckPolicy,
    changeset: &ChangeSet,
    exclusion: &ExclusionMatcher,
) -> CheckResult {
    scope_findings_to_changeset(&mut result, changeset);
    drop_excluded_findings(&mut result, exclusion);

    if result.findings.is_empty() {
        return result;
    }

    if policy.allow_bypass {
        if let Some(reason) = changeset.bypass_reason(&policy.bypass_name) {
            let location = result.findings.iter().find_map(|finding| finding.location.clone());
            result.findings = vec![bypass_applied_finding(&policy.bypass_name, &reason, location)];
            return result;
        }

        let guidance = bypass_failure_guidance(&policy.bypass_name);
        for finding in &mut result.findings {
            finding.remediations.push(guidance.clone());
        }
    }

    // When the policy has an explicit severity override, apply it to all findings
    // unconditionally. When no override is set, either preserve the transform-supplied
    // per-finding severity (declarative checks that opt into SeverityTemplate::Dynamic)
    // or default to Error (built-in and non-dynamic declarative checks).
    if let Some(severity_override) = policy.severity_override {
        for finding in &mut result.findings {
            finding.severity = severity_override;
        }
    } else if !policy.preserve_finding_severity {
        for finding in &mut result.findings {
            finding.severity = Severity::Error;
        }
    }

    result
}

#[cfg(test)]
mod tests;
