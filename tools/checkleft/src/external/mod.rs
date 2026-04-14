use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::path::validate_relative_path;

pub const EXTERNAL_CHECK_RUNTIME_V1: &str = "sandbox-v1";
pub const EXTERNAL_CHECK_EXEC_RUNTIME_V1: &str = "exec-v1";
pub const EXTERNAL_CHECK_API_V1: &str = "v1";
pub const GENERATED_IMPLEMENTATION_PREFIX: &str = "generated:";

pub mod exec_protocol;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckImplementationRef {
    File(PathBuf),
    Generated(String),
}

impl ExternalCheckImplementationRef {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("implementation reference must not be empty");
        }

        if let Some(generated_id) = trimmed.strip_prefix(GENERATED_IMPLEMENTATION_PREFIX) {
            let generated_id = generated_id.trim();
            if generated_id.is_empty() {
                bail!(
                    "generated implementation reference must include an id after `{}`",
                    GENERATED_IMPLEMENTATION_PREFIX
                );
            }
            return Ok(Self::Generated(generated_id.to_owned()));
        }

        let path = PathBuf::from(trimmed);
        validate_relative_path(&path)?;
        Ok(Self::File(path))
    }
}

impl fmt::Display for ExternalCheckImplementationRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Generated(id) => write!(f, "{GENERATED_IMPLEMENTATION_PREFIX}{id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckPackage {
    pub id: String,
    pub runtime: String,
    pub api_version: String,
    pub capabilities: ExternalCheckCapabilities,
    pub implementation: ExternalCheckPackageImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckPackageImplementation {
    Source(ExternalCheckSourcePackage),
    Artifact(ExternalCheckArtifactPackage),
    Exec(ExternalCheckExecPackage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckSourcePackage {
    pub language: String,
    pub entry: String,
    pub build_adapter: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckArtifactPackage {
    pub artifact_path: String,
    pub artifact_sha256: String,
    pub provenance: Option<ExternalCheckArtifactProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckExecPackage {
    pub executable_path: String,
    pub args: Vec<String>,
    pub provenance: Option<ExternalCheckArtifactProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCheckArtifactProvenance {
    pub generator: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalCheckCapabilities {
    pub commands: Vec<String>,
}

pub trait ExternalCheckPackageProvider: Send + Sync {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>>;
}

mod provider;
pub use provider::{
    CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider,
    FileExternalCheckPackageProvider, GeneratedExternalCheckPackageProvider,
    NoopExternalCheckPackageProvider,
};
mod command_policy;
pub use command_policy::ExternalCommandCapabilities;
mod source_builder;
pub use source_builder::{ExternalSourcePackageBuilder, JavaScriptComponentSourcePackageBuilder};
mod runtime;
pub use runtime::{DefaultExternalCheckExecutor, ExternalCheckExecutor, NoopExternalCheckExecutor};

pub fn load_external_check_package_manifest(path: &Path) -> Result<ExternalCheckPackage> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read external check manifest {}", path.display()))?;
    parse_external_check_package_manifest(&contents)
        .with_context(|| format!("invalid external check manifest {}", path.display()))
}

pub fn parse_external_check_package_manifest(contents: &str) -> Result<ExternalCheckPackage> {
    let raw: RawExternalCheckPackage =
        toml::from_str(contents).context("failed to parse external check manifest TOML")?;
    raw.validate()
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExternalCheckMode {
    Source,
    Artifact,
    Exec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalCheckPackage {
    id: String,
    runtime: String,
    api_version: String,
    mode: RawExternalCheckMode,
    #[serde(default)]
    capabilities: Option<RawExternalCheckCapabilities>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    build_adapter: Option<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    artifact_path: Option<String>,
    #[serde(default)]
    artifact_sha256: Option<String>,
    #[serde(default)]
    executable_path: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    provenance: Option<ExternalCheckArtifactProvenance>,
}

impl RawExternalCheckPackage {
    fn validate(self) -> Result<ExternalCheckPackage> {
        let RawExternalCheckPackage {
            id,
            runtime,
            api_version,
            mode,
            capabilities,
            language,
            entry,
            build_adapter,
            sources,
            artifact_path,
            artifact_sha256,
            executable_path,
            args,
            provenance,
        } = self;

        let id = required_non_empty("id", id)?;
        let runtime = required_non_empty("runtime", runtime)?;
        validate_runtime_for_mode(mode, &runtime)?;

        let api_version = required_non_empty("api_version", api_version)?;
        if api_version != EXTERNAL_CHECK_API_V1 {
            bail!(
                "unsupported api_version `{api_version}` (expected `{}`)",
                EXTERNAL_CHECK_API_V1
            );
        }

        let capabilities = validate_capabilities(mode, capabilities)?;
        let implementation = match mode {
            RawExternalCheckMode::Source => {
                ExternalCheckPackageImplementation::Source(validate_source_implementation(
                    language,
                    entry,
                    build_adapter,
                    sources,
                    artifact_path,
                    artifact_sha256,
                    executable_path,
                    args,
                    provenance,
                )?)
            }
            RawExternalCheckMode::Artifact => {
                ExternalCheckPackageImplementation::Artifact(validate_artifact_implementation(
                    language,
                    entry,
                    build_adapter,
                    sources,
                    artifact_path,
                    artifact_sha256,
                    executable_path,
                    args,
                    provenance,
                )?)
            }
            RawExternalCheckMode::Exec => {
                ExternalCheckPackageImplementation::Exec(validate_exec_implementation(
                    language,
                    entry,
                    build_adapter,
                    sources,
                    artifact_path,
                    artifact_sha256,
                    executable_path,
                    args,
                    provenance,
                )?)
            }
        };

        Ok(ExternalCheckPackage {
            id,
            runtime,
            api_version,
            capabilities,
            implementation,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_source_implementation(
    language: Option<String>,
    entry: Option<String>,
    build_adapter: Option<String>,
    sources: Vec<String>,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    executable_path: Option<String>,
    args: Vec<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckSourcePackage> {
    reject_if_present("artifact_path", artifact_path.as_ref())?;
    reject_if_present("artifact_sha256", artifact_sha256.as_ref())?;
    reject_if_present("executable_path", executable_path.as_ref())?;
    reject_if_present_list("args", &args)?;
    reject_if_present("provenance", provenance.as_ref())?;

    let sources = sources
        .into_iter()
        .map(|source| required_relative_path_string("sources[]", source))
        .collect::<Result<Vec<_>>>()?;

    Ok(ExternalCheckSourcePackage {
        language: required_some_non_empty("language", language)?,
        entry: required_some_relative_path_string("entry", entry)?,
        build_adapter: required_some_non_empty("build_adapter", build_adapter)?,
        sources,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_artifact_implementation(
    language: Option<String>,
    entry: Option<String>,
    build_adapter: Option<String>,
    sources: Vec<String>,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    executable_path: Option<String>,
    args: Vec<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckArtifactPackage> {
    reject_if_present("language", language.as_ref())?;
    reject_if_present("entry", entry.as_ref())?;
    reject_if_present("build_adapter", build_adapter.as_ref())?;
    reject_if_present("executable_path", executable_path.as_ref())?;
    reject_if_present_list("args", &args)?;
    if !sources.is_empty() {
        bail!("field `sources` is not allowed in `artifact` mode");
    }

    Ok(ExternalCheckArtifactPackage {
        artifact_path: required_some_relative_path_string("artifact_path", artifact_path)?,
        artifact_sha256: required_some_sha256("artifact_sha256", artifact_sha256)?,
        provenance,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_exec_implementation(
    language: Option<String>,
    entry: Option<String>,
    build_adapter: Option<String>,
    sources: Vec<String>,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    executable_path: Option<String>,
    args: Vec<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckExecPackage> {
    reject_if_present("language", language.as_ref())?;
    reject_if_present("entry", entry.as_ref())?;
    reject_if_present("build_adapter", build_adapter.as_ref())?;
    reject_if_present("artifact_path", artifact_path.as_ref())?;
    reject_if_present("artifact_sha256", artifact_sha256.as_ref())?;
    if !sources.is_empty() {
        bail!("field `sources` is not allowed in `exec` mode");
    }

    let args = args
        .into_iter()
        .map(|arg| required_non_empty("args[]", arg))
        .collect::<Result<Vec<_>>>()?;

    Ok(ExternalCheckExecPackage {
        executable_path: required_some_relative_path_string("executable_path", executable_path)?,
        args,
        provenance,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalCheckCapabilities {
    #[serde(default)]
    commands: Vec<String>,
}

impl RawExternalCheckCapabilities {
    fn validate(&self) -> Result<ExternalCheckCapabilities> {
        let mut seen = HashSet::new();
        let mut commands = Vec::with_capacity(self.commands.len());

        for command in &self.commands {
            let command = required_non_empty("capabilities.commands[]", command.clone())?;
            if command.contains('/') || command.contains('\\') {
                bail!(
                    "command `{command}` must be a bare command name, not a path in `capabilities.commands`"
                );
            }
            if command.chars().any(char::is_whitespace) {
                bail!("command `{command}` must not contain whitespace");
            }
            if !seen.insert(command.clone()) {
                bail!("duplicate command `{command}` in `capabilities.commands`");
            }
            commands.push(command);
        }

        Ok(ExternalCheckCapabilities { commands })
    }
}

fn validate_runtime_for_mode(mode: RawExternalCheckMode, runtime: &str) -> Result<()> {
    let expected = match mode {
        RawExternalCheckMode::Source | RawExternalCheckMode::Artifact => EXTERNAL_CHECK_RUNTIME_V1,
        RawExternalCheckMode::Exec => EXTERNAL_CHECK_EXEC_RUNTIME_V1,
    };
    if runtime != expected {
        bail!("unsupported runtime `{runtime}` for `{mode}` mode (expected `{expected}`)");
    }
    Ok(())
}

fn validate_capabilities(
    mode: RawExternalCheckMode,
    capabilities: Option<RawExternalCheckCapabilities>,
) -> Result<ExternalCheckCapabilities> {
    match (mode, capabilities) {
        (RawExternalCheckMode::Exec, Some(_)) => {
            bail!("field `capabilities` is not allowed in `exec` mode");
        }
        (_, Some(raw)) => raw.validate(),
        (_, None) => Ok(ExternalCheckCapabilities::default()),
    }
}

impl fmt::Display for RawExternalCheckMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => write!(f, "source"),
            Self::Artifact => write!(f, "artifact"),
            Self::Exec => write!(f, "exec"),
        }
    }
}

fn reject_if_present<T>(field_name: &str, value: Option<&T>) -> Result<()> {
    if value.is_some() {
        bail!("field `{field_name}` is not allowed for this package mode");
    }
    Ok(())
}

fn reject_if_present_list<T>(field_name: &str, values: &[T]) -> Result<()> {
    if !values.is_empty() {
        bail!("field `{field_name}` is not allowed for this package mode");
    }
    Ok(())
}

fn required_some_non_empty(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_non_empty(field_name, value)
}

fn required_some_relative_path_string(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_relative_path_string(field_name, value)
}

fn required_some_sha256(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_sha256(field_name, value)
}

fn required_non_empty(field_name: &str, value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("field `{field_name}` must not be empty");
    }
    Ok(trimmed.to_owned())
}

fn required_sha256(field_name: &str, value: String) -> Result<String> {
    let normalized = required_non_empty(field_name, value)?;
    let is_valid = normalized.len() == 64
        && normalized
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !is_valid {
        bail!(
            "field `{field_name}` must be a canonical sha256 digest (64 lowercase hex characters)"
        );
    }
    Ok(normalized)
}

fn required_relative_path_string(field_name: &str, value: String) -> Result<String> {
    let normalized = required_non_empty(field_name, value)?;
    validate_relative_path(Path::new(&normalized))
        .with_context(|| format!("field `{field_name}` must be a safe relative path"))?;
    Ok(normalized)
}

#[cfg(test)]
mod tests;
