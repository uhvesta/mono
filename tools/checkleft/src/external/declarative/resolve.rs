//! Framework-owned binary resolution.
//!
//! A declared binary requirement is resolved to a concrete executable path by the
//! *framework*, not by any guest code. Two resolvers are supported:
//!
//! - [`BinaryBinding::Path`] — a direct path or PATH name, used as-is. The
//!   portable fallback: standalone checkleft (no Bazel workspace) always has this.
//! - [`BinaryBinding::Bazel`] — a Bazel label, built then resolved to its
//!   executable. **Environment-conditional**: it requires a Bazel workspace, so it
//!   works in-repo but not in standalone checkleft. It reuses the *same* resolver
//!   the built-in buildifier check uses ([`resolve_bazel_target_executable`]),
//!   which proves the framework can own what the built-in hand-rolled.
//!
//! A CHECKS-config override may substitute a different binding per declared name,
//! keeping CHECKS.yaml thin (enable + repo-specific overrides only) while the
//! definition lives in the package manifest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::checks::buildifier::resolve_bazel_target_executable;

use super::{BinaryBinding, BinaryRequirement};

/// Resolve every declared binary to a concrete path, honoring CHECKS-config
/// overrides. Returns a map keyed by the declared name.
pub fn resolve_all(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut resolved = BTreeMap::new();
    for (name, requirement) in needs {
        let binding = override_binding(name, config)
            .transpose()
            .with_context(|| format!("invalid config override for binary `{name}`"))?
            .unwrap_or_else(|| requirement.default.clone());
        let path = resolve_one(repo_root, name, &binding)
            .with_context(|| format!("failed to resolve declared binary `{name}`"))?;
        resolved.insert(name.clone(), path);
    }
    Ok(resolved)
}

fn resolve_one(repo_root: &Path, name: &str, binding: &BinaryBinding) -> Result<PathBuf> {
    match binding {
        // Used as-is: may be a PATH name (resolved by the OS at spawn) or a path.
        BinaryBinding::Path(path) => Ok(PathBuf::from(path)),
        BinaryBinding::Bazel(target) => {
            resolve_bazel_target_executable(repo_root, target).with_context(|| {
                format!(
                    "bazel resolution of `{name}` target `{target}` failed; this resolver needs a \
                     Bazel workspace — set a `path` override for standalone use"
                )
            })
        }
    }
}

/// Read an optional binding override from `config` at `needs.<name>.{path|bazel}`.
fn override_binding(name: &str, config: &toml::Value) -> Option<Result<BinaryBinding>> {
    let entry = config.get("needs")?.get(name)?;
    let bazel = entry.get("bazel").and_then(toml::Value::as_str);
    let path = entry.get("path").and_then(toml::Value::as_str);
    match (bazel, path) {
        (Some(_), Some(_)) => Some(Err(anyhow::anyhow!(
            "override for `{name}` sets both `bazel` and `path`; set exactly one"
        ))),
        (Some(bazel), None) => Some(Ok(BinaryBinding::Bazel(bazel.to_owned()))),
        (None, Some(path)) => Some(Ok(BinaryBinding::Path(path.to_owned()))),
        (None, None) => Some(Err(anyhow::anyhow!(
            "override for `{name}` must set `bazel` or `path`"
        ))),
    }
    .map(|result| {
        result.and_then(|binding| match &binding {
            BinaryBinding::Path(value) | BinaryBinding::Bazel(value) if value.trim().is_empty() => {
                bail!("override for `{name}` must not be empty")
            }
            _ => Ok(binding),
        })
    })
}
