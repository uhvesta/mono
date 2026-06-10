use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::path::validate_relative_path;

/// Runtime tag for the `component` tier (WebAssembly Component Model). The
/// `component` manifest mode selects this runtime.
pub const EXTERNAL_CHECK_COMPONENT_RUNTIME_V1: &str = "component-v1";
/// Runtime tag for the `declarative` tier (config-only, framework-owned
/// invocation + declarative transforms). Subsumes the former `exec-v1` tier.
pub const EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1: &str = "declarative-v1";
pub const EXTERNAL_CHECK_API_V1: &str = "v1";
pub const GENERATED_IMPLEMENTATION_PREFIX: &str = "generated:";
/// Prefix selecting a first-party check definition embedded in the checkleft
/// binary (resolved by [`BundledExternalCheckPackageProvider`]). `bundled:<name>`
/// names a def under `tools/checkleft/checks/<name>/`.
pub const BUNDLED_IMPLEMENTATION_PREFIX: &str = "bundled:";

pub mod declarative;

pub use declarative::{ExternalCheckDeclarativePackage, run_declarative_check};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckImplementationRef {
    File(PathBuf),
    Generated(String),
    /// A first-party definition embedded in the checkleft binary, named by its
    /// bundle key (the directory name under `tools/checkleft/checks/`). Zero
    /// install for the target repo. Resolved by
    /// [`BundledExternalCheckPackageProvider`].
    Bundled(String),
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

        if let Some(bundled_name) = trimmed.strip_prefix(BUNDLED_IMPLEMENTATION_PREFIX) {
            let bundled_name = bundled_name.trim();
            if bundled_name.is_empty() {
                bail!(
                    "bundled implementation reference must include a name after `{}`",
                    BUNDLED_IMPLEMENTATION_PREFIX
                );
            }
            validate_bundled_name(bundled_name)?;
            return Ok(Self::Bundled(bundled_name.to_owned()));
        }

        let path = PathBuf::from(trimmed);
        validate_relative_path(&path)?;
        Ok(Self::File(path))
    }
}

/// A bundled definition is named by a single path segment (the directory under
/// `tools/checkleft/checks/`). Reject separators / traversal so a `bundled:` ref
/// can never escape the embedded tree.
pub(crate) fn validate_bundled_name(name: &str) -> Result<()> {
    if name.contains('/') || name.contains('\\') {
        bail!("bundled implementation name `{name}` must be a single segment, not a path");
    }
    if name == "." || name == ".." {
        bail!("bundled implementation name `{name}` is not a valid definition name");
    }
    Ok(())
}

impl fmt::Display for ExternalCheckImplementationRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Generated(id) => write!(f, "{GENERATED_IMPLEMENTATION_PREFIX}{id}"),
            Self::Bundled(name) => write!(f, "{BUNDLED_IMPLEMENTATION_PREFIX}{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckPackage {
    pub id: String,
    pub runtime: String,
    pub api_version: String,
    pub implementation: ExternalCheckPackageImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckPackageImplementation {
    /// The `component` tier: a WebAssembly Component Model artifact with
    /// capability-scoped file access and resource limits.
    Component(ExternalCheckComponentPackage),
    /// The `declarative` tier: framework-owned invocations + declarative
    /// transforms. Subsumes the former `exec` tier (via the `passthrough`
    /// transform).
    Declarative(ExternalCheckDeclarativePackage),
}

/// A WebAssembly Component Model check package parsed from a `mode = "component"`
/// manifest, or assembled by the bundled provider from embedded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckComponentPackage {
    /// Repo-root-relative path to the `.wasm` artifact on disk. Empty for
    /// bundled packages where the bytes are embedded via `artifact_bytes`.
    pub artifact_path: String,
    pub artifact_sha256: String,
    /// Bytes embedded at compile time for first-party (bundled) components.
    /// When `Some`, the executor loads these bytes directly instead of reading
    /// `artifact_path` from disk. `artifact_path` is empty in this case.
    pub artifact_bytes: Option<&'static [u8]>,
    /// The check name to pass to `run-check`. For single-check manifests this
    /// always equals the package `id`. For multi-check component bundles it
    /// selects the specific export within the component.
    pub check_name: String,
    pub limits: Option<ExternalCheckComponentLimits>,
    /// Optional allowlist of check IDs exported by this component. When present,
    /// must agree with what `list-checks` returns (defense-in-depth).
    pub checks: Option<Vec<String>>,
    pub provenance: Option<ExternalCheckArtifactProvenance>,
}

/// Per-manifest resource limits for a component-mode check. Values are clamped
/// by a host ceiling at execution time so an out-of-tree manifest cannot grant
/// itself unbounded resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckComponentLimits {
    pub timeout_ms: Option<u64>,
    pub max_memory_mb: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCheckArtifactProvenance {
    pub generator: Option<String>,
    pub target: Option<String>,
}

pub trait ExternalCheckPackageProvider: Send + Sync {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>>;
}

mod bundled;
pub use bundled::{BundledExternalCheckPackageProvider, bundled_check_names};
mod provider;
pub use provider::{
    CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider,
    FileExternalCheckPackageProvider, GeneratedExternalCheckPackageProvider,
    NoopExternalCheckPackageProvider,
};
mod component_bindings;
mod runtime;
pub use runtime::{
    ComponentAotCache, DefaultExternalCheckExecutor, ExternalCheckExecutor, NoopExternalCheckExecutor,
};
pub mod sandbox;
pub use sandbox::{AccessScope, HostCeiling, SandboxResult, create_sandbox};

pub fn load_external_check_package_manifest(path: &Path) -> Result<ExternalCheckPackage> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read external check manifest {}", path.display()))?;
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    parse_external_check_manifest(&contents, extension)
        .with_context(|| format!("invalid external check manifest {}", path.display()))
}

/// Parse a manifest given its contents and file extension. YAML manifests use
/// the declarative schema; everything else (TOML) goes through the
/// artifact/declarative TOML schema. Shared by the on-disk file loader and the
/// in-binary bundled provider so both honor identical format rules.
pub fn parse_external_check_manifest(contents: &str, extension: &str) -> Result<ExternalCheckPackage> {
    if matches!(extension, "yaml" | "yml") {
        parse_declarative_check_manifest(contents)
    } else {
        parse_external_check_package_manifest(contents)
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
            implementation: ExternalCheckPackageImplementation::Declarative(
                declarative::validate_declarative_implementation(declarative_fields)?,
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExternalCheckMode {
    Component,
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
    artifact_path: Option<String>,
    #[serde(default)]
    artifact_sha256: Option<String>,
    // Component-mode fields.
    #[serde(default)]
    limits: Option<RawExternalCheckComponentLimits>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    provenance: Option<ExternalCheckArtifactProvenance>,
    // Declarative-mode fields. Declared explicitly (not flattened — `flatten` is
    // incompatible with `deny_unknown_fields`) so the existing single-parse +
    // unknown-field rejection still holds. They are rejected in component mode
    // and required in declarative mode.
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    needs: std::collections::BTreeMap<String, declarative::RawBinaryRequirement>,
    #[serde(default)]
    invocations: Vec<declarative::RawInvocation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalCheckComponentLimits {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_memory_mb: Option<u64>,
}

impl RawExternalCheckPackage {
    fn validate(self) -> Result<ExternalCheckPackage> {
        let RawExternalCheckPackage {
            id,
            runtime,
            api_version,
            mode,
            artifact_path,
            artifact_sha256,
            limits,
            checks,
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

        let implementation = match mode {
            RawExternalCheckMode::Component => {
                reject_declarative_fields(&declarative)?;
                ExternalCheckPackageImplementation::Component(validate_component_implementation(
                    &id,
                    artifact_path,
                    artifact_sha256,
                    limits,
                    checks,
                    provenance,
                )?)
            }
            RawExternalCheckMode::Declarative => {
                reject_if_present("artifact_path", artifact_path.as_ref())?;
                reject_if_present("artifact_sha256", artifact_sha256.as_ref())?;
                reject_if_present("limits", limits.as_ref())?;
                reject_if_present_list("checks", &checks)?;
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
            implementation,
        })
    }
}

fn validate_component_implementation(
    id: &str,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    limits: Option<RawExternalCheckComponentLimits>,
    checks: Vec<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckComponentPackage> {
    let validated_limits = limits.as_ref().map(|raw| ExternalCheckComponentLimits {
        timeout_ms: raw.timeout_ms,
        max_memory_mb: raw.max_memory_mb,
    });
    let checks_allowlist = if checks.is_empty() {
        None
    } else {
        Some(checks)
    };
    Ok(ExternalCheckComponentPackage {
        artifact_path: required_some_relative_path_string("artifact_path", artifact_path)?,
        artifact_sha256: required_some_sha256("artifact_sha256", artifact_sha256)?,
        artifact_bytes: None,
        check_name: id.to_owned(),
        limits: validated_limits,
        checks: checks_allowlist,
        provenance,
    })
}

fn validate_runtime_for_mode(mode: RawExternalCheckMode, runtime: &str) -> Result<()> {
    let expected = match mode {
        RawExternalCheckMode::Component => EXTERNAL_CHECK_COMPONENT_RUNTIME_V1,
        RawExternalCheckMode::Declarative => EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1,
    };
    if runtime != expected {
        bail!("unsupported runtime `{runtime}` for `{mode}` mode (expected `{expected}`)");
    }
    Ok(())
}

/// Reject declarative-only fields in non-declarative modes.
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
            Self::Component => write!(f, "component"),
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
