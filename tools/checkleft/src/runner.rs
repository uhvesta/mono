use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use tokio::task::JoinSet;

use crate::bypass::{bypass_applied_finding, bypass_failure_guidance, bypass_name_for_check_id};
use crate::check::{CheckRegistry, ConfiguredCheck};
use crate::config::{CheckConfig, CheckConfigOrigin, ConfigDiagnostic, ConfigResolver};
use crate::exclusion::ExclusionStatus;
use crate::external::{
    ExternalCheckExecutor, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalCheckPackageProvider, NoopExternalCheckExecutor, NoopExternalCheckPackageProvider,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};
use tracing::info;

struct ScheduledCheckRun {
    configured_check_id: String,
    source_path: PathBuf,
    execution: ScheduledExecution,
    policy: EffectiveCheckPolicy,
    config: toml::Value,
    changeset: ChangeSet,
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

pub struct Runner {
    registry: Arc<CheckRegistry>,
    resolver: Arc<ConfigResolver>,
    source_tree: Arc<dyn SourceTree>,
    external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
    external_executor: Arc<dyn ExternalCheckExecutor>,
}

impl Runner {
    pub fn new(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
    ) -> Self {
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
        let scheduled = self.schedule_runs(changeset)?;
        info!(
            scheduled_runs = scheduled.runs.len(),
            diagnostics = scheduled.diagnostics.len(),
            "scheduled check execution"
        );

        let mut results = scheduled.diagnostics;
        let mut join_set = JoinSet::new();
        for run in scheduled.runs {
            match run.execution {
                ScheduledExecution::BuiltInConfigured { check } => {
                    let source_tree = Arc::clone(&self.source_tree);
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_policy = run.policy;
                    let source_path = run.source_path;
                    let file_count = run_changeset.changed_files.len();
                    info!(
                        check_id = %configured_check_id,
                        file_count,
                        "running built-in check"
                    );

                    join_set.spawn(async move {
                        check
                            .run(&run_changeset, source_tree.as_ref())
                            .await
                            .map(|mut result| {
                                // Report findings under the configured instance id.
                                result.check_id = configured_check_id.clone();
                                apply_policy_to_result(result, &run_policy, &run_changeset)
                            })
                            .map_err(|err| (configured_check_id, source_path, err))
                    });
                }
                ScheduledExecution::BuiltInMissing {
                    implementation_check_id,
                } => {
                    results.push(CheckResult {
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
                    });
                }
                ScheduledExecution::ExternalResolved { package } => {
                    let external_executor = Arc::clone(&self.external_executor);
                    let source_tree = Arc::clone(&self.source_tree);
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_config = run.config;
                    let run_policy = run.policy;
                    let source_path = run.source_path;
                    let file_count = run_changeset.changed_files.len();
                    info!(
                        check_id = %configured_check_id,
                        package_id = %package.id,
                        file_count,
                        "running external check"
                    );

                    join_set.spawn(async move {
                        // The executor is synchronous but wasmtime-wasi internally calls
                        // block_on, which panics if a Tokio runtime is already active on
                        // the thread.  spawn_blocking moves execution onto a thread-pool
                        // thread where no runtime is running.
                        let check_id_clone = configured_check_id.clone();
                        let source_path_clone = source_path.clone();
                        tokio::task::spawn_blocking(move || {
                            external_executor
                                .execute(
                                    &package,
                                    &run_changeset,
                                    source_tree.as_ref(),
                                    &run_config,
                                )
                                .map(|mut result| {
                                    result.check_id = configured_check_id.clone();
                                    apply_policy_to_result(result, &run_policy, &run_changeset)
                                })
                                .map_err(|err| (configured_check_id, source_path, err))
                        })
                        .await
                        .unwrap_or_else(|e| {
                            Err((check_id_clone, source_path_clone, anyhow!("executor panicked: {e}")))
                        })
                    });
                }
                ScheduledExecution::Invalid {
                    message,
                    remediation,
                } => {
                    results.push(CheckResult {
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
                    });
                }
            }
        }

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(Ok(result)) => results.push(result),
                Ok(Err((check_id, source_path, err))) => {
                    results.push(CheckResult {
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
                    });
                }
                Err(join_err) => {
                    return Err(anyhow!("runner task failed to execute: {join_err}"));
                }
            }
        }

        results.extend(self.audit_stale_exclusions(changeset).await?);

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
                // Only checks that have a native built-in can participate in
                // stale-exclusion auditing.  Pure external/component checks that
                // have no native counterpart are silently skipped here; bundled
                // component checks that also have a native built-in (transition
                // period) use the built-in's evaluate_exclusion.
                let Some(built_in) = self.registry.get(&check.check) else {
                    continue;
                };
                let Ok(configured) =
                    built_in.configure_scoped(&check.config, check.source_path.parent())
                else {
                    // Misconfiguration is surfaced by the normal pass; stay quiet here.
                    continue;
                };
                let declared = configured.declared_exclusions();
                if declared.is_empty() {
                    continue;
                }

                for exclusion in declared {
                    // Empty dependency set => dependency unknown => never audit (fail safe).
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
                    let dedupe_key = (
                        check.id.clone(),
                        check.source_path.clone(),
                        exclusion.entry.clone(),
                    );
                    if !audited.insert(dedupe_key) {
                        continue;
                    }

                    let status = match configured
                        .evaluate_exclusion(&exclusion, self.source_tree.as_ref())
                        .await
                    {
                        Ok(status) => status,
                        Err(err) => {
                            // Fail safe: an evaluation error is never reported as stale.
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

                    let line = self.locate_exclusion_line(
                        &check.source_path,
                        &exclusion.entry,
                        &mut config_text_cache,
                    );
                    findings_by_check
                        .entry(check.id.clone())
                        .or_default()
                        .push(Finding {
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
                if let ScheduledExecution::Invalid { message, .. } =
                    self.resolve_scheduled_execution(check)
                {
                    resolution_errors.insert(check.id.clone(), message);
                }
                checks.insert(check.id.clone());
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
            let details = config_diagnostics
                .into_iter()
                .collect::<Vec<_>>()
                .join("\n- ");
            bail!("failed to resolve checks configuration:\n- {details}");
        }

        Ok(checks.into_iter().collect())
    }

    fn schedule_runs(&self, changeset: &ChangeSet) -> Result<ScheduledRuns> {
        info!(
            changed_files = changeset.changed_files.len(),
            "scheduling checks"
        );
        let mut grouped_runs: BTreeMap<
            (String, String, String, String, String),
            ScheduledCheckRun,
        > = BTreeMap::new();
        // Dedup key for config diagnostics:
        // (check_id, path, line, column, message, remediation).
        type DiagnosticGroupKey = (String, PathBuf, Option<u32>, Option<u32>, String, String);
        let mut grouped_diagnostics: BTreeMap<DiagnosticGroupKey, CheckResult> = BTreeMap::new();

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
            for check in resolved.enabled() {
                let policy = self.resolve_effective_policy(check);
                let config_fingerprint = toml::to_string(&check.config).unwrap_or_default();
                let implementation_fingerprint = check
                    .implementation
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let policy_fingerprint = policy.fingerprint();
                let key = (
                    check.id.clone(),
                    check.check.clone(),
                    implementation_fingerprint,
                    config_fingerprint,
                    policy_fingerprint,
                );

                let entry = grouped_runs
                    .entry(key)
                    .or_insert_with(|| ScheduledCheckRun {
                        configured_check_id: check.id.clone(),
                        source_path: check.source_path.clone(),
                        execution: self.resolve_scheduled_execution(check),
                        policy,
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

        Ok(ScheduledRuns {
            runs: grouped_runs.into_values().collect(),
            diagnostics: grouped_diagnostics.into_values().collect(),
        })
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
                    remediation: Some(
                        "Fix this check's `config` block in the CHECKS file.".to_owned(),
                    ),
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
                    "Set `check = ...` to match the external package `id` or update the package manifest."
                        .to_owned(),
                ),
            };
        }

        if check.origin == CheckConfigOrigin::ExternalUrl
            && matches!(
                &package.implementation,
                ExternalCheckPackageImplementation::Declarative(_)
            )
        {
            // The declarative runtime runs real binaries the framework selects
            // (this is the path the former `exec` tier folded into); letting a
            // remote-fetched config drive that is the same trust level as shipping
            // a binary, so it is rejected from `external_checks_url`.
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

fn should_skip_file(changed_file: &ChangedFile, resolved: &crate::config::ResolvedChecks) -> bool {
    is_checks_config_file(&changed_file.path) && !resolved.include_config_files()
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

fn apply_policy_to_result(
    mut result: CheckResult,
    policy: &EffectiveCheckPolicy,
    changeset: &ChangeSet,
) -> CheckResult {
    if result.findings.is_empty() {
        return result;
    }

    if policy.allow_bypass {
        if let Some(reason) = changeset.bypass_reason(&policy.bypass_name) {
            let location = result
                .findings
                .iter()
                .find_map(|finding| finding.location.clone());
            result.findings = vec![bypass_applied_finding(
                &policy.bypass_name,
                &reason,
                location,
            )];
            return result;
        }

        let guidance = bypass_failure_guidance(&policy.bypass_name);
        for finding in &mut result.findings {
            finding.remediations.push(guidance.clone());
        }
    }

    if let Some(severity_override) = policy.severity_override {
        for finding in &mut result.findings {
            finding.severity = severity_override;
        }
    }

    result
}

#[cfg(test)]
mod tests;
