use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::bypass::bypass_name_for_check_id;
use crate::external::ExternalCheckImplementationRef;
use crate::output::{Location, Severity};
use crate::path::validate_relative_path;
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use tracing::info;

const CHECKS_FILE_NAME_YAML: &str = "CHECKS.yaml";
const CHECKS_FILE_NAME_TOML: &str = "CHECKS.toml";
const CHECKS_CONFIG_DIAGNOSTIC_ID: &str = "checks-config";
const CHECKLEFT_HTTP_USER_AGENT: &str = "checkleft-cli";
const EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS: u32 = 5;
#[cfg(not(test))]
const EXTERNAL_CHECKS_FETCH_BASE_DELAY: Duration = Duration::from_millis(250);
#[cfg(test)]
const EXTERNAL_CHECKS_FETCH_BASE_DELAY: Duration = Duration::from_millis(5);
#[cfg(not(test))]
const EXTERNAL_CHECKS_FETCH_404_BASE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const EXTERNAL_CHECKS_FETCH_404_BASE_DELAY: Duration = Duration::from_millis(20);
const EXTERNAL_CHECKS_MAX_CHAIN_DEPTH: usize = 8;

fn ensure_rustls_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConfig {
    pub check: String,
    pub id: String,
    pub source_path: PathBuf,
    /// Directory containing the CHECKS.toml that declared this check, relative to repo root.
    /// Empty path means the repo root itself.
    pub config_dir: PathBuf,
    pub origin: CheckConfigOrigin,
    pub implementation: Option<ExternalCheckImplementationRef>,
    pub enabled: bool,
    pub policy: CheckPolicyConfig,
    pub config: toml::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConfigOrigin {
    Local,
    ExternalFile,
    ExternalUrl,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckPolicyConfig {
    pub severity: Option<Severity>,
    pub allow_bypass: Option<bool>,
    pub bypass_name: Option<String>,
    /// Per-check override of the stale-exclusion audit mode. `None` inherits the
    /// resolved global default (see [`ResolvedChecks::stale_exclusion_mode`]).
    pub stale_exclusion_mode: Option<StaleExclusionMode>,
}

/// How the stale-exclusion audit reports a dead exclusion. Defaults to
/// [`Warn`](StaleExclusionMode::Warn); a repo can set it to
/// [`Error`](StaleExclusionMode::Error) to fail CI on dead exclusions, or
/// [`Off`](StaleExclusionMode::Off) to disable the audit entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleExclusionMode {
    Off,
    Warn,
    Error,
}

impl Default for StaleExclusionMode {
    fn default() -> Self {
        Self::Warn
    }
}

impl StaleExclusionMode {
    /// The finding severity for this mode, or `None` when the audit is disabled.
    pub fn severity(self) -> Option<Severity> {
        match self {
            Self::Off => None,
            Self::Warn => Some(Severity::Warning),
            Self::Error => Some(Severity::Error),
        }
    }
}

fn parse_stale_exclusion_mode(raw: &str) -> Result<StaleExclusionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Ok(StaleExclusionMode::Off),
        "warn" | "warning" => Ok(StaleExclusionMode::Warn),
        "error" => Ok(StaleExclusionMode::Error),
        _ => bail!("expected one of `off`, `warning`, or `error`"),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedChecks {
    checks_by_id: BTreeMap<String, CheckConfig>,
    diagnostics: Vec<ConfigDiagnostic>,
    include_config_files: bool,
    stale_exclusion_mode: StaleExclusionMode,
}

impl ResolvedChecks {
    pub fn iter(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values()
    }

    pub fn enabled(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values().filter(|check| check.enabled)
    }

    pub fn get(&self, id: &str) -> Option<&CheckConfig> {
        self.checks_by_id.get(id)
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &ConfigDiagnostic> {
        self.diagnostics.iter()
    }

    pub fn include_config_files(&self) -> bool {
        self.include_config_files
    }

    /// The resolved global default mode for the stale-exclusion audit at this point in
    /// the config hierarchy. Per-check `policy.stale_exclusion_severity` overrides it.
    pub fn stale_exclusion_mode(&self) -> StaleExclusionMode {
        self.stale_exclusion_mode
    }

    fn upsert(&mut self, check: CheckConfig) {
        self.checks_by_id.insert(check.id.clone(), check);
    }

    fn push_diagnostic(&mut self, diagnostic: ConfigDiagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub check_id: String,
    pub message: String,
    pub location: Location,
    pub remediation: Option<String>,
}

#[derive(Debug)]
pub struct ConfigResolver {
    root: PathBuf,
    external_root_configs: Vec<LoadedChecksFile>,
    resolution_cache: Mutex<HashMap<PathBuf, ResolvedChecks>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigResolverOptions {
    pub external_checks_file: Option<String>,
    pub external_checks_url: Option<String>,
}

impl ConfigResolver {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = canonicalize_root(root.into())?;
        Ok(Self {
            root,
            external_root_configs: Vec::new(),
            resolution_cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn new_with_options(
        root: impl Into<PathBuf>,
        options: ConfigResolverOptions,
    ) -> Result<Self> {
        let root = canonicalize_root(root.into())?;
        let external_root_configs = if let Some(external_checks_file) =
            normalize_optional_cli_value(options.external_checks_file)
        {
            if normalize_optional_cli_value(options.external_checks_url).is_some() {
                bail!("only one of external checks file or external checks URL may be configured");
            }
            vec![load_external_checks_file_path(
                &root,
                &external_checks_file,
            )?]
        } else if let Some(external_checks_url) =
            normalize_optional_cli_value(options.external_checks_url)
        {
            load_external_checks_chain(&external_checks_url).await?
        } else if let Some(external_checks_url) =
            discover_root_external_checks_url_for_prefetch(&root)?
        {
            load_external_checks_chain(&external_checks_url).await?
        } else {
            Vec::new()
        };

        Ok(Self {
            root,
            external_root_configs,
            resolution_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn resolve_for_file(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;
        info!(path = %file_path.display(), "resolving checks for file");

        self.resolve_for_dir(file_path.parent().unwrap_or(Path::new("")))
    }

    #[cfg(feature = "benchmarking")]
    #[doc(hidden)]
    pub fn resolve_for_file_without_cache(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;

        self.resolve_for_dir_without_cache(file_path.parent().unwrap_or(Path::new("")))
    }

    fn resolve_for_dir(&self, relative_dir: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(relative_dir)?;

        if let Some(cached) = self.cached_resolution(relative_dir) {
            return Ok(cached);
        }

        let mut resolved = if relative_dir.as_os_str().is_empty() {
            self.base_resolution()?
        } else {
            self.resolve_for_dir(relative_dir.parent().unwrap_or(Path::new("")))?
        };

        self.apply_local_config(&mut resolved, relative_dir);
        self.store_cached_resolution(relative_dir, &resolved);
        Ok(resolved)
    }

    #[cfg(feature = "benchmarking")]
    fn resolve_for_dir_without_cache(&self, relative_dir: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(relative_dir)?;

        let mut resolved = if relative_dir.as_os_str().is_empty() {
            self.base_resolution()?
        } else {
            self.resolve_for_dir_without_cache(relative_dir.parent().unwrap_or(Path::new("")))?
        };

        self.apply_local_config(&mut resolved, relative_dir);
        Ok(resolved)
    }

    fn cached_resolution(&self, relative_dir: &Path) -> Option<ResolvedChecks> {
        self.resolution_cache
            .lock()
            .expect("config resolution cache poisoned")
            .get(relative_dir)
            .cloned()
    }

    fn store_cached_resolution(&self, relative_dir: &Path, resolved: &ResolvedChecks) {
        self.resolution_cache
            .lock()
            .expect("config resolution cache poisoned")
            .insert(relative_dir.to_path_buf(), resolved.clone());
    }

    fn base_resolution(&self) -> Result<ResolvedChecks> {
        let mut resolved = ResolvedChecks::default();
        for external_checks_file in &self.external_root_configs {
            apply_external_checks_file(&mut resolved, external_checks_file)?;
        }
        Ok(resolved)
    }

    fn apply_local_config(&self, resolved: &mut ResolvedChecks, relative_dir: &Path) {
        let config_abs_dir = self.root.join(relative_dir);
        let Some(config_path) = resolve_checks_file_path(&config_abs_dir) else {
            return;
        };
        info!(path = %config_path.display(), "loading checks config");
        let config_relative_path = config_path
            .strip_prefix(&self.root)
            .unwrap_or(config_path.as_path())
            .to_path_buf();
        let check_config_dir = config_relative_path
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();

        let checks_file = match parse_checks_file(&config_path, &config_relative_path) {
            Ok(checks_file) => checks_file,
            Err(diagnostic) => {
                resolved.push_diagnostic(diagnostic);
                return;
            }
        };
        apply_local_settings(
            resolved,
            &checks_file.settings,
            config_abs_dir == self.root,
            &config_relative_path,
        );
        for check in checks_file.checks {
            let configured_id = check.id;
            let implementation = if check.enabled {
                match parse_check_implementation(
                    check.implementation.as_deref(),
                    &configured_id,
                    None,
                ) {
                    Ok(implementation) => implementation,
                    Err(err) => {
                        resolved.push_diagnostic(config_check_diagnostic(
                            configured_id.clone(),
                            config_relative_path.clone(),
                            err.to_string(),
                        ));
                        continue;
                    }
                }
            } else {
                None
            };
            let policy =
                match parse_policy_config(&configured_id, &check.policy, check.enabled, None) {
                    Ok(policy) => policy,
                    Err(err) => {
                        resolved.push_diagnostic(config_check_diagnostic(
                            configured_id.clone(),
                            config_relative_path.clone(),
                            err.to_string(),
                        ));
                        continue;
                    }
                };
            resolved.upsert(CheckConfig {
                check: check.check.unwrap_or_else(|| configured_id.clone()),
                id: configured_id,
                source_path: config_relative_path.clone(),
                config_dir: check_config_dir.clone(),
                origin: CheckConfigOrigin::Local,
                implementation,
                enabled: check.enabled,
                policy,
                config: check.config,
            });
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ParsedChecksFile {
    #[serde(default)]
    settings: ParsedSettings,
    #[serde(default)]
    checks: Vec<ParsedCheckConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ParsedSettings {
    #[serde(default)]
    include_config_files: Option<bool>,
    #[serde(default)]
    external_checks_url: Option<String>,
    #[serde(default)]
    stale_exclusion_severity: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ParsedCheckConfig {
    id: String,
    #[serde(default)]
    check: Option<String>,
    #[serde(default)]
    implementation: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    policy: ParsedCheckPolicyConfig,
    #[serde(default = "empty_toml_table")]
    config: toml::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ParsedCheckPolicyConfig {
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    allow_bypass: Option<bool>,
    #[serde(default)]
    bypass_name: Option<String>,
    #[serde(default)]
    stale_exclusion_severity: Option<String>,
}

#[derive(Debug)]
struct LoadedChecksFile {
    origin: CheckConfigOrigin,
    source_label: String,
    source_path: PathBuf,
    parsed: ParsedChecksFile,
}

fn parse_checks_file(
    path: &Path,
    relative_path: &Path,
) -> std::result::Result<ParsedChecksFile, ConfigDiagnostic> {
    let contents = fs::read_to_string(path).map_err(|err| {
        config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            relative_path.to_path_buf(),
            format!("failed to read checks config: {err}"),
            None,
            None,
            Some("Fix this CHECKS file so checkleft can load it.".to_owned()),
        )
    })?;
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

    match extension {
        "yaml" | "yml" => {
            serde_yaml::from_str(&contents).map_err(|err| yaml_parse_diagnostic(relative_path, err))
        }
        "toml" => toml::from_str(&contents)
            .map_err(|err| toml_parse_diagnostic(relative_path, &contents, err)),
        _ => Err(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            relative_path.to_path_buf(),
            "unsupported checks config extension (expected .yaml or .toml)".to_owned(),
            None,
            None,
            Some("Rename the file to CHECKS.yaml or CHECKS.toml.".to_owned()),
        )),
    }
}

fn parse_checks_contents(
    contents: &str,
    extension: &str,
    source_label: &str,
) -> Result<ParsedChecksFile> {
    match extension {
        "yaml" | "yml" => serde_yaml::from_str(contents)
            .with_context(|| format!("failed to parse {source_label}")),
        "toml" => {
            toml::from_str(contents).with_context(|| format!("failed to parse {source_label}"))
        }
        _ => bail!(
            "unsupported checks config extension for {} (expected .yaml or .toml)",
            source_label
        ),
    }
}

fn enabled_default() -> bool {
    true
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn parse_check_implementation(
    implementation: Option<&str>,
    check_id: &str,
    config_source: Option<&str>,
) -> Result<Option<ExternalCheckImplementationRef>> {
    let Some(implementation) = implementation else {
        return Ok(None);
    };
    let implementation = ExternalCheckImplementationRef::parse(implementation).with_context(
        || match config_source {
            Some(config_source) => {
                format!("invalid `implementation` for check `{check_id}` in {config_source}")
            }
            None => format!("invalid `implementation` for check `{check_id}`"),
        },
    )?;
    Ok(Some(implementation))
}

fn parse_policy_config(
    check_id: &str,
    policy: &ParsedCheckPolicyConfig,
    enabled: bool,
    config_source: Option<&str>,
) -> Result<CheckPolicyConfig> {
    if !enabled {
        return Ok(CheckPolicyConfig::default());
    }

    let severity = match policy.severity.as_deref() {
        Some(raw) => Some(
            parse_policy_severity(raw).with_context(|| match config_source {
                Some(config_source) => {
                    format!("invalid `policy.severity` for check `{check_id}` in {config_source}")
                }
                None => format!("invalid `policy.severity` for check `{check_id}`"),
            })?,
        ),
        None => None,
    };

    let bypass_name = policy
        .bypass_name
        .clone()
        .map(|raw| normalize_bypass_name(raw, check_id));

    let stale_exclusion_mode = match policy.stale_exclusion_severity.as_deref() {
        Some(raw) => Some(parse_stale_exclusion_mode(raw).with_context(|| match config_source {
            Some(config_source) => format!(
                "invalid `policy.stale_exclusion_severity` for check `{check_id}` in {config_source}"
            ),
            None => format!("invalid `policy.stale_exclusion_severity` for check `{check_id}`"),
        })?),
        None => None,
    };

    Ok(CheckPolicyConfig {
        severity,
        allow_bypass: policy.allow_bypass,
        bypass_name,
        stale_exclusion_mode,
    })
}

fn parse_policy_severity(raw: &str) -> Result<Severity> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        "info" => Ok(Severity::Info),
        _ => bail!("expected one of `error`, `warning`, or `info`"),
    }
}

fn normalize_bypass_name(raw: String, check_id: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return bypass_name_for_check_id(check_id);
    }
    if trimmed.to_ascii_uppercase().starts_with("BYPASS_") {
        return trimmed.to_ascii_uppercase();
    }

    bypass_name_for_check_id(trimmed)
}

fn yaml_parse_diagnostic(relative_path: &Path, err: serde_yaml::Error) -> ConfigDiagnostic {
    let location = err.location();
    config_file_diagnostic(
        CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
        relative_path.to_path_buf(),
        format!("failed to parse checks config: {err}"),
        location.as_ref().map(|location| location.line() as u32),
        location.as_ref().map(|location| location.column() as u32),
        Some("Fix YAML syntax so checkleft can load this CHECKS file.".to_owned()),
    )
}

fn toml_parse_diagnostic(
    relative_path: &Path,
    contents: &str,
    err: toml::de::Error,
) -> ConfigDiagnostic {
    let (line, column) = err
        .span()
        .map(|span| offset_to_line_column(contents, span.start))
        .map(|(line, column)| (Some(line), Some(column)))
        .unwrap_or((None, None));

    config_file_diagnostic(
        CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
        relative_path.to_path_buf(),
        format!("failed to parse checks config: {err}"),
        line,
        column,
        Some("Fix TOML syntax so checkleft can load this CHECKS file.".to_owned()),
    )
}

fn config_check_diagnostic(
    check_id: String,
    source_path: PathBuf,
    message: String,
) -> ConfigDiagnostic {
    config_file_diagnostic(
        check_id,
        source_path,
        message,
        None,
        None,
        Some("Fix this check entry in the CHECKS file.".to_owned()),
    )
}

fn config_file_diagnostic(
    check_id: String,
    path: PathBuf,
    message: String,
    line: Option<u32>,
    column: Option<u32>,
    remediation: Option<String>,
) -> ConfigDiagnostic {
    ConfigDiagnostic {
        check_id,
        message,
        location: Location { path, line, column },
        remediation,
    }
}

fn offset_to_line_column(contents: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut column = 1u32;
    for (index, ch) in contents.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    (line, column)
}

fn canonicalize_root(root: PathBuf) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
    if !root.is_dir() {
        bail!(
            "config resolver root is not a directory: {}",
            root.display()
        );
    }

    Ok(root)
}

fn apply_local_settings(
    resolved: &mut ResolvedChecks,
    settings: &ParsedSettings,
    is_root_config: bool,
    source_path: &Path,
) {
    if let Some(include_config_files) = settings.include_config_files {
        resolved.include_config_files = include_config_files;
    }

    if let Some(raw) = settings.stale_exclusion_severity.as_deref() {
        match parse_stale_exclusion_mode(raw) {
            Ok(mode) => resolved.stale_exclusion_mode = mode,
            Err(error) => resolved.push_diagnostic(config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                format!("invalid `settings.stale_exclusion_severity`: {error}"),
                None,
                None,
                Some(
                    "Set `settings.stale_exclusion_severity` to `off`, `warning`, or `error`."
                        .to_owned(),
                ),
            )),
        }
    }

    let Some(external_checks_url) = settings.external_checks_url.as_deref() else {
        return;
    };
    if !is_root_config {
        resolved.push_diagnostic(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            source_path.to_path_buf(),
            "`settings.external_checks_url` is only supported in the repository root config"
                .to_owned(),
            None,
            None,
            Some("Remove `settings.external_checks_url` from child CHECKS files.".to_owned()),
        ));
        return;
    }

    if let Err(error) = validate_external_checks_url(external_checks_url, None) {
        resolved.push_diagnostic(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            source_path.to_path_buf(),
            format!("invalid `settings.external_checks_url`: {error}"),
            None,
            None,
            Some("Set `settings.external_checks_url` to a valid absolute URL.".to_owned()),
        ));
    }
}

fn apply_external_checks_file(
    resolved: &mut ResolvedChecks,
    external_checks_file: &LoadedChecksFile,
) -> Result<()> {
    if let Some(include_config_files) = external_checks_file.parsed.settings.include_config_files {
        resolved.include_config_files = include_config_files;
    }

    if let Some(raw) = external_checks_file
        .parsed
        .settings
        .stale_exclusion_severity
        .as_deref()
    {
        resolved.stale_exclusion_mode = parse_stale_exclusion_mode(raw).with_context(|| {
            format!(
                "invalid `settings.stale_exclusion_severity` in {}",
                external_checks_file.source_label
            )
        })?;
    }

    for check in &external_checks_file.parsed.checks {
        let configured_id = check.id.clone();
        let implementation = if check.enabled {
            let implementation = parse_check_implementation(
                check.implementation.as_deref(),
                &configured_id,
                Some(&external_checks_file.source_label),
            )?;
            validate_external_root_check_implementation(
                external_checks_file.origin,
                implementation.as_ref(),
                &configured_id,
                &external_checks_file.source_label,
            )?;
            implementation
        } else {
            None
        };
        let policy = parse_policy_config(
            &configured_id,
            &check.policy,
            check.enabled,
            Some(&external_checks_file.source_label),
        )?;
        resolved.upsert(CheckConfig {
            check: check.check.clone().unwrap_or_else(|| configured_id.clone()),
            id: configured_id,
            source_path: external_checks_file.source_path.clone(),
            config_dir: PathBuf::new(),
            origin: external_checks_file.origin,
            implementation,
            enabled: check.enabled,
            policy,
            config: check.config.clone(),
        });
    }

    Ok(())
}

fn discover_root_external_checks_url_for_prefetch(root: &Path) -> Result<Option<String>> {
    let Some(config_path) = resolve_checks_file_path(root) else {
        return Ok(None);
    };
    let relative_path = config_path
        .strip_prefix(root)
        .unwrap_or(config_path.as_path())
        .to_path_buf();
    let Ok(checks_file) = parse_checks_file(&config_path, &relative_path) else {
        return Ok(None);
    };
    let Some(external_checks_url) = checks_file.settings.external_checks_url else {
        return Ok(None);
    };
    let Some(external_checks_url) = normalize_optional_cli_value(Some(external_checks_url)) else {
        return Ok(None);
    };
    if resolve_external_checks_url(&external_checks_url, None).is_err() {
        return Ok(None);
    }
    Ok(Some(external_checks_url))
}

fn load_external_checks_file_path(
    root: &Path,
    external_checks_file: &str,
) -> Result<LoadedChecksFile> {
    let resolved_path = resolve_external_checks_file_path(root, external_checks_file)?;
    let source_label = resolved_path.display().to_string();
    let parsed = parse_checks_contents_from_path(&resolved_path, &source_label)?;

    Ok(LoadedChecksFile {
        origin: CheckConfigOrigin::ExternalFile,
        source_label,
        source_path: resolved_path,
        parsed,
    })
}

fn normalize_optional_cli_value(raw: Option<String>) -> Option<String> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn validate_external_checks_url(raw_url: &str, base_url: Option<&reqwest::Url>) -> Result<()> {
    resolve_external_checks_url(raw_url, base_url).map(|_| ())
}

fn resolve_external_checks_file_path(root: &Path, raw_path: &str) -> Result<PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        bail!("external checks file path must not be empty");
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        return Ok(path);
    }

    validate_relative_path(&path)
        .context("external checks file path must be a safe relative path")?;
    Ok(root.join(path))
}

fn parse_checks_contents_from_path(path: &Path, source_label: &str) -> Result<ParsedChecksFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read external checks config {source_label}"))?;
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    parse_checks_contents(&contents, extension, source_label)
        .with_context(|| format!("failed to parse external checks config {source_label}"))
}

fn normalize_configured_external_checks_url(
    raw: Option<String>,
    source_label: &str,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("`settings.external_checks_url` in {source_label} must not be empty");
    }
    Ok(Some(trimmed.to_owned()))
}

async fn load_external_checks_chain(external_checks_url: &str) -> Result<Vec<LoadedChecksFile>> {
    info!(external_checks_url, "loading external checks config chain");
    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .user_agent(CHECKLEFT_HTTP_USER_AGENT)
        .build()
        .context("failed to build HTTP client for external checks config")?;
    let mut seen_urls = HashSet::new();
    let mut loaded = Vec::new();
    let mut next_url = Some(resolve_external_checks_url(external_checks_url, None)?);

    while let Some(url) = next_url {
        if loaded.len() >= EXTERNAL_CHECKS_MAX_CHAIN_DEPTH {
            bail!(
                "external checks config chain exceeded {} entries while loading {}",
                EXTERNAL_CHECKS_MAX_CHAIN_DEPTH,
                external_checks_url
            );
        }
        if !seen_urls.insert(url.as_str().to_owned()) {
            bail!("external checks config cycle detected at {}", url);
        }

        let fetched = fetch_external_checks_file(&client, url.clone()).await?;
        next_url = normalize_configured_external_checks_url(
            fetched.parsed.settings.external_checks_url.clone(),
            &fetched.source_label,
        )?
        .map(|nested_url| resolve_external_checks_url(&nested_url, Some(&url)))
        .transpose()?;
        loaded.push(fetched);
    }

    loaded.reverse();
    Ok(loaded)
}

fn resolve_external_checks_url(
    raw_url: &str,
    base_url: Option<&reqwest::Url>,
) -> Result<reqwest::Url> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        bail!("external checks URL must not be empty");
    }

    let parsed = match base_url {
        Some(base_url) => base_url
            .join(trimmed)
            .or_else(|_| reqwest::Url::parse(trimmed)),
        None => reqwest::Url::parse(trimmed),
    };

    parsed.with_context(|| format!("invalid external checks URL `{trimmed}`"))
}

fn validate_external_root_check_implementation(
    origin: CheckConfigOrigin,
    implementation: Option<&ExternalCheckImplementationRef>,
    check_id: &str,
    source_label: &str,
) -> Result<()> {
    let Some(implementation) = implementation else {
        return Ok(());
    };

    if origin == CheckConfigOrigin::ExternalFile
        && matches!(implementation, ExternalCheckImplementationRef::File(_))
    {
        bail!(
            "invalid `implementation` for check `{check_id}` in {source_label}: external checks files may only use `generated:` implementations"
        );
    }

    Ok(())
}

async fn fetch_external_checks_file(
    client: &reqwest::Client,
    url: reqwest::Url,
) -> Result<LoadedChecksFile> {
    let mut last_retryable_error = None;

    for attempt in 1..=EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
        info!(
            url = %url,
            attempt,
            max_attempts = EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS,
            "fetching external checks config"
        );
        match client.get(url.clone()).send().await {
            Ok(response) => {
                let status = response.status();
                if status == StatusCode::NOT_FOUND {
                    if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                        bail!(
                            "external checks config {} returned 404 Not Found after {} attempts",
                            url,
                            EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS
                        );
                    }
                    tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                    continue;
                }

                if !status.is_success() {
                    let response_error =
                        format!("external checks config {} returned {}", url, status);
                    if is_retryable_http_status(status) {
                        last_retryable_error = Some(response_error);
                        if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                            break;
                        }
                        tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                        continue;
                    }
                    bail!("{response_error}");
                }

                let contents = match response.text().await {
                    Ok(contents) => contents,
                    Err(error) => {
                        last_retryable_error = Some(format!(
                            "failed to read external checks config {} response body: {error}",
                            url
                        ));
                        if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                            break;
                        }
                        tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                        continue;
                    }
                };
                let extension = Path::new(url.path())
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("");
                let parsed = parse_checks_contents(&contents, extension, url.as_str())
                    .with_context(|| format!("failed to parse external checks config {url}"))?;
                return Ok(LoadedChecksFile {
                    origin: CheckConfigOrigin::ExternalUrl,
                    source_label: url.to_string(),
                    source_path: PathBuf::from(url.as_str()),
                    parsed,
                });
            }
            Err(error) => {
                last_retryable_error = Some(format!(
                    "failed to retrieve external checks config {}: {error}",
                    url
                ));
                if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                    break;
                }
                tokio::time::sleep(external_checks_retry_delay(
                    attempt,
                    StatusCode::REQUEST_TIMEOUT,
                ))
                .await;
            }
        }
    }

    let message = last_retryable_error
        .unwrap_or_else(|| format!("failed to retrieve external checks config {}", url));
    bail!(
        "{message} after {} attempts",
        EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS
    )
}

fn is_retryable_http_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn external_checks_retry_delay(attempt: u32, status: StatusCode) -> Duration {
    let base_delay = if status == StatusCode::NOT_FOUND {
        EXTERNAL_CHECKS_FETCH_404_BASE_DELAY
    } else {
        EXTERNAL_CHECKS_FETCH_BASE_DELAY
    };
    base_delay.saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)))
}

fn resolve_checks_file_path(dir: &Path) -> Option<PathBuf> {
    let yaml_path = dir.join(CHECKS_FILE_NAME_YAML);
    if yaml_path.exists() {
        return Some(yaml_path);
    }

    let toml_path = dir.join(CHECKS_FILE_NAME_TOML);
    if toml_path.exists() {
        return Some(toml_path);
    }

    None
}

#[cfg(test)]
mod tests;
