use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::path::validate_relative_path;

/// Runtime tag for the `wasm` tier (sandboxed pure computation). The `wasm`
/// manifest mode selects this runtime.
pub const EXTERNAL_CHECK_RUNTIME_V1: &str = "sandbox-v1";
/// Runtime tag for the `declarative` tier (config-only, framework-owned
/// invocation + declarative transforms). Subsumes the former `exec-v1` tier.
pub const EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1: &str = "declarative-v1";
pub const EXTERNAL_CHECK_API_V1: &str = "v1";
pub const GENERATED_IMPLEMENTATION_PREFIX: &str = "generated:";

pub mod declarative;

pub use declarative::{ExternalCheckDeclarativePackage, run_declarative_check};

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
    /// The `wasm` tier: a sandboxed wasm artifact (pure computation).
    Artifact(ExternalCheckArtifactPackage),
    /// The `declarative` tier: framework-owned invocations + declarative
    /// transforms. Subsumes the former `exec` tier (via the `passthrough`
    /// transform).
    Declarative(ExternalCheckDeclarativePackage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckArtifactPackage {
    pub artifact_path: String,
    pub artifact_sha256: String,
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
mod runtime;
pub use runtime::{DefaultExternalCheckExecutor, ExternalCheckExecutor, NoopExternalCheckExecutor};

pub fn load_external_check_package_manifest(path: &Path) -> Result<ExternalCheckPackage> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read external check manifest {}", path.display()))?;
    let is_yaml = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml" | "yml")
    );
    if is_yaml {
        parse_declarative_check_manifest(&contents)
            .with_context(|| format!("invalid declarative external check manifest {}", path.display()))
    } else {
        parse_external_check_package_manifest(&contents)
            .with_context(|| format!("invalid external check manifest {}", path.display()))
    }
}

pub fn parse_external_check_package_manifest(contents: &str) -> Result<ExternalCheckPackage> {
    let raw: RawExternalCheckPackage =
        toml::from_str(contents).context("failed to parse external check manifest TOML")?;
    raw.validate()
}

/// Parse a declarative check package manifest from YAML. Declarative manifests
/// are YAML (as opposed to the TOML used by `artifact`/`exec` packages) — they
/// have a richer schema (invocations/transforms) that reads naturally as YAML.
pub fn parse_declarative_check_manifest(contents: &str) -> Result<ExternalCheckPackage> {
    let raw: RawDeclarativeCheckManifest =
        serde_yaml::from_str(contents).context("failed to parse declarative check manifest YAML")?;
    raw.validate()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeclarativeCheckManifest {
    id: String,
    mode: String,
    runtime: String,
    api_version: String,
    applies_to: Vec<String>,
    #[serde(default)]
    needs: std::collections::BTreeMap<String, declarative::RawBinaryRequirement>,
    #[serde(default)]
    invocations: Vec<declarative::RawInvocation>,
}

impl RawDeclarativeCheckManifest {
    fn validate(self) -> Result<ExternalCheckPackage> {
        let id = required_non_empty("id", self.id)?;
        let runtime = required_non_empty("runtime", self.runtime)?;
        let api_version = required_non_empty("api_version", self.api_version)?;

        if self.mode != "declarative" {
            bail!(
                "declarative manifest `mode` must be `declarative`, got `{}`",
                self.mode
            );
        }
        if runtime != EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1 {
            bail!(
                "unsupported runtime `{runtime}` (expected `{EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1}`)"
            );
        }
        if api_version != EXTERNAL_CHECK_API_V1 {
            bail!(
                "unsupported api_version `{api_version}` (expected `{EXTERNAL_CHECK_API_V1}`)"
            );
        }

        let declarative_fields = declarative::RawDeclarativeFields {
            applies_to: self.applies_to,
            needs: self.needs,
            invocations: self.invocations,
        };

        Ok(ExternalCheckPackage {
            id,
            runtime,
            api_version,
            capabilities: ExternalCheckCapabilities::default(),
            implementation: ExternalCheckPackageImplementation::Declarative(
                declarative::validate_declarative_implementation(declarative_fields)?,
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExternalCheckMode {
    Wasm,
    Declarative,
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
    artifact_path: Option<String>,
    #[serde(default)]
    artifact_sha256: Option<String>,
    #[serde(default)]
    executable_path: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    provenance: Option<ExternalCheckArtifactProvenance>,
    // Declarative-mode fields. Declared explicitly (not flattened — `flatten` is
    // incompatible with `deny_unknown_fields`) so the existing single-parse +
    // unknown-field rejection still holds. They are rejected in artifact/exec
    // modes and required in declarative mode.
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    needs: std::collections::BTreeMap<String, declarative::RawBinaryRequirement>,
    #[serde(default)]
    invocations: Vec<declarative::RawInvocation>,
}

impl RawExternalCheckPackage {
    fn validate(self) -> Result<ExternalCheckPackage> {
        let RawExternalCheckPackage {
            id,
            runtime,
            api_version,
            mode,
            capabilities,
            artifact_path,
            artifact_sha256,
            executable_path,
            args,
            provenance,
            applies_to,
            needs,
            invocations,
        } = self;
        let declarative = declarative::RawDeclarativeFields {
            applies_to,
            needs,
            invocations,
        };

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
            RawExternalCheckMode::Wasm => {
                reject_declarative_fields(&declarative)?;
                ExternalCheckPackageImplementation::Artifact(validate_artifact_implementation(
                    artifact_path,
                    artifact_sha256,
                    executable_path,
                    args,
                    provenance,
                )?)
            }
            RawExternalCheckMode::Declarative => {
                reject_if_present("artifact_path", artifact_path.as_ref())?;
                reject_if_present("artifact_sha256", artifact_sha256.as_ref())?;
                reject_if_present("executable_path", executable_path.as_ref())?;
                reject_if_present_list("args", &args)?;
                reject_if_present("provenance", provenance.as_ref())?;
                ExternalCheckPackageImplementation::Declarative(
                    declarative::validate_declarative_implementation(declarative)?,
                )
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

fn validate_artifact_implementation(
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    executable_path: Option<String>,
    args: Vec<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckArtifactPackage> {
    reject_if_present("executable_path", executable_path.as_ref())?;
    reject_if_present_list("args", &args)?;

    Ok(ExternalCheckArtifactPackage {
        artifact_path: required_some_relative_path_string("artifact_path", artifact_path)?,
        artifact_sha256: required_some_sha256("artifact_sha256", artifact_sha256)?,
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
        RawExternalCheckMode::Wasm => EXTERNAL_CHECK_RUNTIME_V1,
        RawExternalCheckMode::Declarative => EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1,
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
        // Capabilities are a wasm-guest command-grant concept; the declarative
        // tier runs binaries directly (framework-owned), so a `capabilities`
        // block is meaningless there.
        (RawExternalCheckMode::Declarative, Some(_)) => {
            bail!("field `capabilities` is not allowed in `declarative` mode");
        }
        (_, Some(raw)) => raw.validate(),
        (_, None) => Ok(ExternalCheckCapabilities::default()),
    }
}

/// Reject declarative-only fields in artifact/exec modes.
fn reject_declarative_fields(declarative: &declarative::RawDeclarativeFields) -> Result<()> {
    if !declarative.is_empty() {
        bail!(
            "fields `applies_to`/`needs`/`invocations` are only allowed in `declarative` mode"
        );
    }
    Ok(())
}

impl fmt::Display for RawExternalCheckMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wasm => write!(f, "wasm"),
            Self::Declarative => write!(f, "declarative"),
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
