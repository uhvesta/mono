use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::path::validate_relative_path;
use tracing::info;

use super::{
    ExternalCheckImplementationRef, ExternalCheckPackage, ExternalCheckPackageProvider,
    GENERATED_IMPLEMENTATION_PREFIX, load_external_check_package_manifest,
};

#[derive(Debug, Default)]
pub struct NoopExternalCheckPackageProvider;

impl ExternalCheckPackageProvider for NoopExternalCheckPackageProvider {
    fn resolve(
        &self,
        _implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
        Ok(None)
    }
}

#[derive(Debug)]
pub struct FileExternalCheckPackageProvider {
    root: PathBuf,
}

impl FileExternalCheckPackageProvider {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
        if !root.is_dir() {
            bail!(
                "external package provider root is not a directory: {}",
                root.display()
            );
        }
        Ok(Self { root })
    }
}

impl ExternalCheckPackageProvider for FileExternalCheckPackageProvider {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
        let ExternalCheckImplementationRef::File(relative_path) = implementation_ref else {
            return Ok(None);
        };

        let manifest_path = self.root.join(relative_path);
        info!(path = %manifest_path.display(), "resolving file external package manifest");
        if !manifest_path.exists() {
            return Ok(None);
        }

        load_external_check_package_manifest(&manifest_path).map(Some)
    }
}

#[derive(Debug)]
pub struct GeneratedExternalCheckPackageProvider {
    packages_by_generated_id: BTreeMap<String, ExternalCheckPackage>,
}

impl GeneratedExternalCheckPackageProvider {
    pub fn from_index_path(root: &Path, index_path: &Path) -> Result<Self> {
        let full_index_path = resolve_rooted_path(root, index_path, "generated index path")?;
        info!(path = %full_index_path.display(), "loading generated external package index");
        let index_contents = fs::read_to_string(&full_index_path).with_context(|| {
            format!(
                "failed to read generated external package index {}",
                full_index_path.display()
            )
        })?;

        let raw_index: RawGeneratedExternalCheckPackageIndex = toml::from_str(&index_contents)
            .with_context(|| {
                format!(
                    "failed to parse generated external package index TOML {}",
                    full_index_path.display()
                )
            })?;
        if let Some(version) = raw_index.version
            && version != 1 {
                bail!(
                    "unsupported generated external package index version `{version}` in {} (expected 1)",
                    full_index_path.display()
                );
            }

        let index_dir = full_index_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());

        let mut packages_by_generated_id = BTreeMap::new();
        for (entry_idx, raw_entry) in raw_index.packages.into_iter().enumerate() {
            let generated_id = raw_entry.parse_generated_id().with_context(|| {
                format!("index entry [{}] has invalid implementation", entry_idx)
            })?;
            let manifest_path = raw_entry.parse_manifest_path().with_context(|| {
                format!("index entry [{}] has invalid manifest path", entry_idx)
            })?;
            let manifest_full_path = index_dir.join(manifest_path);

            let package = load_external_check_package_manifest(&manifest_full_path).with_context(|| {
                format!(
                    "failed to load manifest for generated implementation `{generated_id}` from index {}",
                    full_index_path.display()
                )
            })?;

            if packages_by_generated_id
                .insert(generated_id.clone(), package)
                .is_some()
            {
                bail!(
                    "duplicate generated implementation `{generated_id}` in index {}",
                    full_index_path.display()
                );
            }
        }

        Ok(Self {
            packages_by_generated_id,
        })
    }
}

impl ExternalCheckPackageProvider for GeneratedExternalCheckPackageProvider {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
        let ExternalCheckImplementationRef::Generated(generated_id) = implementation_ref else {
            return Ok(None);
        };
        Ok(self.packages_by_generated_id.get(generated_id).cloned())
    }
}

pub struct ConfiguredExternalCheckPackageProvider {
    pub name: String,
    pub provider: Arc<dyn ExternalCheckPackageProvider>,
}

impl ConfiguredExternalCheckPackageProvider {
    pub fn new(name: impl Into<String>, provider: Arc<dyn ExternalCheckPackageProvider>) -> Self {
        Self {
            name: name.into(),
            provider,
        }
    }
}

pub struct CompositeExternalCheckPackageProvider {
    providers: Vec<ConfiguredExternalCheckPackageProvider>,
}

impl CompositeExternalCheckPackageProvider {
    pub fn new(providers: Vec<ConfiguredExternalCheckPackageProvider>) -> Self {
        Self { providers }
    }
}

impl ExternalCheckPackageProvider for CompositeExternalCheckPackageProvider {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
        info!(
            implementation_ref = %implementation_ref,
            providers = self.providers.len(),
            "resolving external package reference"
        );
        let mut matches = Vec::new();

        for configured in &self.providers {
            let package = configured
                .provider
                .resolve(implementation_ref)
                .with_context(|| {
                    format!(
                        "external package provider `{}` failed resolving `{implementation_ref}`",
                        configured.name
                    )
                })?;
            if let Some(package) = package {
                matches.push((configured.name.as_str(), package));
            }
        }

        if matches.len() > 1 {
            let provider_names = matches
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "implementation reference `{implementation_ref}` resolved by multiple providers: {provider_names}"
            );
        }

        Ok(matches.pop().map(|(_, package)| package))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGeneratedExternalCheckPackageIndex {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    packages: Vec<RawGeneratedExternalCheckPackageEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGeneratedExternalCheckPackageEntry {
    implementation: String,
    manifest: String,
}

impl RawGeneratedExternalCheckPackageEntry {
    fn parse_generated_id(&self) -> Result<String> {
        let implementation_ref = ExternalCheckImplementationRef::parse(&self.implementation)?;
        match implementation_ref {
            ExternalCheckImplementationRef::Generated(id) => Ok(id),
            ExternalCheckImplementationRef::File(_) => bail!(
                "generated index `packages[].implementation` must use the `{}` prefix",
                GENERATED_IMPLEMENTATION_PREFIX
            ),
        }
    }

    fn parse_manifest_path(&self) -> Result<PathBuf> {
        parse_relative_path("packages[].manifest", &self.manifest)
    }
}

fn parse_relative_path(field_name: &str, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        bail!("field `{field_name}` must not be empty");
    }
    let path = PathBuf::from(value);
    validate_relative_path(&path)
        .with_context(|| format!("field `{field_name}` must be a safe relative path"))?;
    Ok(path)
}

fn resolve_rooted_path(root: &Path, path: &Path, field_name: &str) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    validate_relative_path(path)
        .with_context(|| format!("field `{field_name}` must be a safe relative path"))?;
    Ok(root.join(path))
}
