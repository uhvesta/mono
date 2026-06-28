use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::input::SourceTree;
use crate::path::validate_relative_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    pub package: PackageIdentity,
    pub includes: BTreeMap<String, PackageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    CheckPackage,
    VersionSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRef {
    pub source: String,
    pub version: String,
    pub sha256: Option<String>,
}

impl PackageManifest {
    pub fn read_from_tree(tree: &dyn SourceTree, checkleft_root: &Path) -> Result<Self> {
        validate_relative_path(checkleft_root)?;
        let manifest_path = checkleft_root.join("package.toml");
        let bytes = tree
            .read_file(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?;
        let contents =
            String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", manifest_path.display()))?;
        Self::parse(&contents).with_context(|| format!("failed to parse {}", manifest_path.display()))
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let raw: RawManifest = toml::from_str(contents).context("invalid package.toml")?;
        let package = raw.package.context("package.toml must contain [package]")?;
        if package.name.trim().is_empty() {
            bail!("[package].name must not be empty");
        }
        if package.version.trim().is_empty() {
            bail!("[package].version must not be empty");
        }

        let includes = validate_refs("includes", raw.includes)?;
        match package.kind {
            PackageKind::CheckPackage if !includes.is_empty() => {
                bail!("check_package manifests must not declare [includes]; select packages in CHECKS.yaml instead");
            }
            PackageKind::VersionSet if includes.is_empty() => {
                bail!("version_set manifests must declare at least one [includes.<name>] entry");
            }
            _ => {}
        }

        Ok(Self {
            package: PackageIdentity {
                name: package.name,
                version: package.version,
                kind: package.kind,
            },
            includes,
        })
    }
}

fn validate_refs(section: &'static str, refs: BTreeMap<String, RawPackageRef>) -> Result<BTreeMap<String, PackageRef>> {
    let mut result = BTreeMap::new();
    for (alias, package_ref) in refs {
        if alias.trim().is_empty() {
            bail!("[{section}] aliases must not be empty");
        }
        if package_ref.source.trim().is_empty() {
            bail!("[{section}.{alias}].source must not be empty");
        }
        if package_ref.version.trim().is_empty() {
            bail!("[{section}.{alias}].version must not be empty");
        }
        validate_source_uri(section, &alias, &package_ref.source)?;
        validate_exact_version(section, &alias, &package_ref.version)?;
        if !package_ref.source.starts_with("path://") && package_ref.sha256.is_none() {
            bail!("[{section}.{alias}].sha256 is required for fetched package refs");
        }
        if let Some(hash) = &package_ref.sha256
            && !is_canonical_sha256(hash)
        {
            bail!("[{section}.{alias}].sha256 must be a canonical sha256 digest");
        }
        result.insert(
            alias,
            PackageRef {
                source: package_ref.source,
                version: package_ref.version,
                sha256: package_ref.sha256,
            },
        );
    }
    Ok(result)
}

fn validate_source_uri(section: &str, alias: &str, source: &str) -> Result<()> {
    let Some((scheme, rest)) = source.split_once("://") else {
        bail!("[{section}.{alias}].source must use registry://, git://, or path://");
    };
    match scheme {
        "registry" | "git" => {
            if rest.is_empty() {
                bail!("[{section}.{alias}].source must include a non-empty {scheme} target");
            }
        }
        "path" => {
            let path = Path::new(rest);
            validate_relative_path(path)
                .with_context(|| format!("[{section}.{alias}].source path:// value must be repo-root-relative"))?;
            if rest.is_empty() {
                bail!("[{section}.{alias}].source path:// value must not be empty");
            }
        }
        _ => bail!("[{section}.{alias}].source uses unsupported scheme `{scheme}`"),
    }
    Ok(())
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_exact_version(section: &str, alias: &str, version: &str) -> Result<()> {
    if version.contains('*')
        || version.contains('^')
        || version.contains('~')
        || version.contains('<')
        || version.contains('>')
    {
        bail!("[{section}.{alias}].version must be an exact version pin");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    package: Option<RawPackage>,
    #[serde(default)]
    includes: BTreeMap<String, RawPackageRef>,
}

#[derive(Debug, Deserialize)]
struct RawPackage {
    name: String,
    version: String,
    #[serde(default)]
    kind: PackageKind,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPackageRef {
    source: String,
    version: String,
    #[serde(default)]
    sha256: Option<String>,
}

impl Default for PackageKind {
    fn default() -> Self {
        Self::CheckPackage
    }
}

impl<'de> Deserialize<'de> for PackageKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "check_package" => Ok(Self::CheckPackage),
            "version_set" => Ok(Self::VersionSet),
            other => Err(serde::de::Error::custom(format!("unsupported package kind `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_package_manifest_metadata() {
        let manifest = PackageManifest::parse(
            r#"
[package]
name = "myorg/repo-checks"
version = "0.1.0"
"#,
        )
        .expect("parse manifest");

        assert_eq!(manifest.package.name, "myorg/repo-checks");
        assert_eq!(manifest.package.kind, PackageKind::CheckPackage);
        assert!(manifest.includes.is_empty());
    }

    #[test]
    fn rejects_check_package_includes() {
        let err = PackageManifest::parse(
            r#"
[package]
name = "myorg/repo-checks"
version = "0.1.0"

[includes.bad]
source = "registry://checkleft-hub/core"
version = "1.2.3"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        )
        .expect_err("check package includes must fail");

        assert!(err.to_string().contains("must not declare [includes]"), "{err:#}");
    }

    #[test]
    fn rejects_empty_version_set_manifest() {
        let err = PackageManifest::parse(
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"
"#,
        )
        .expect_err("empty version set must fail");

        assert!(err.to_string().contains("must declare at least one"), "{err:#}");
    }

    #[test]
    fn parses_version_set_manifest_includes() {
        let manifest = PackageManifest::parse(
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.core]
source = "registry://checkleft-hub/core"
version = "1.2.3"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        )
        .expect("parse manifest");

        assert_eq!(manifest.package.kind, PackageKind::VersionSet);
        assert_eq!(manifest.includes["core"].source, "registry://checkleft-hub/core");
        assert_eq!(
            manifest.includes["core"].sha256.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn rejects_fetched_includes_without_hash() {
        let err = PackageManifest::parse(
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.core]
source = "registry://checkleft-hub/core"
version = "1.2.3"
"#,
        )
        .expect_err("missing hash must fail");

        assert!(err.to_string().contains("sha256"), "{err:#}");
    }

    #[test]
    fn rejects_non_canonical_include_hashes() {
        let err = PackageManifest::parse(
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.core]
source = "registry://checkleft-hub/core"
version = "1.2.3"
sha256 = "ABC123"
"#,
        )
        .expect_err("non-canonical hash must fail");

        assert!(err.to_string().contains("canonical sha256 digest"), "{err:#}");
    }

    #[test]
    fn rejects_path_includes_with_parent_traversal() {
        let err = PackageManifest::parse(
            r#"
[package]
name = "myorg/repo-checks"
version = "0.1.0"

[includes.bad]
source = "path://../other/checkleft"
version = "0.0.0"
"#,
        )
        .expect_err("parent traversal must fail");

        assert!(err.to_string().contains("repo-root-relative"), "{err:#}");
    }
}
