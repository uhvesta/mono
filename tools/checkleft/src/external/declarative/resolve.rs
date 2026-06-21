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
//!
//! When a Bazel binding is declared alongside a `fallback.path`, hermetic resolution
//! failure is non-fatal: the framework logs a loud warning to stderr naming the
//! reason and the resolved fallback binary, then continues. Without a fallback
//! declared, failure is an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

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
        let override_binding = override_binding(name, config)
            .transpose()
            .with_context(|| format!("invalid config override for binary `{name}`"))?;

        let path = if let Some(binding) = override_binding {
            resolve_binding(repo_root, &binding)
                .with_context(|| format!("failed to resolve declared binary `{name}`"))?
        } else {
            resolve_requirement(repo_root, name, requirement)?
        };

        resolved.insert(name.clone(), path);
    }
    Ok(resolved)
}

/// Resolve using the default binding, falling back if declared and bazel fails.
fn resolve_requirement(repo_root: &Path, name: &str, requirement: &BinaryRequirement) -> Result<PathBuf> {
    match &requirement.default {
        BinaryBinding::Path(_) => resolve_binding(repo_root, &requirement.default)
            .with_context(|| format!("failed to resolve declared binary `{name}`")),
        BinaryBinding::Bazel(target) => match resolve_bazel_target_executable(repo_root, target) {
            Ok(path) => Ok(path),
            Err(err) => {
                if let Some(fallback) = &requirement.fallback {
                    let fallback_path = resolve_binding(repo_root, fallback).with_context(|| {
                        format!("hermetic resolution of `{name}` failed AND fallback resolution also failed")
                    })?;
                    let version = binary_version_string(&fallback_path);
                    eprintln!(
                        "warning: checkleft: `{name}`: hermetic toolchain unresolved ({err:#}); \
                         falling back to PATH binary `{}` ({})",
                        fallback_path.display(),
                        if version.is_empty() {
                            "version unknown".to_owned()
                        } else {
                            version
                        }
                    );
                    Ok(fallback_path)
                } else {
                    Err(err).with_context(|| {
                        format!(
                            "failed to resolve declared binary `{name}`; \
                             declare `needs.{name}.fallback.path` for non-Bazel environments"
                        )
                    })
                }
            }
        },
    }
}

fn resolve_binding(repo_root: &Path, binding: &BinaryBinding) -> Result<PathBuf> {
    match binding {
        BinaryBinding::Path(path) => Ok(resolve_path_binding(repo_root, path)),
        BinaryBinding::Bazel(target) => resolve_bazel_target_executable(repo_root, target),
    }
}

/// Resolve a `path` binding to a concrete program path.
///
/// - A **bare name** (no path separator, e.g. `buildifier`) is left as-is so the
///   OS resolves it via `PATH` at spawn time.
/// - An **absolute path** is used as-is.
/// - A **relative path with a separator** (e.g. a `bazel-bin/…/launcher` produced
///   by the `local_check` bazel rule, which is how the folded `exec` tier ships a
///   binary) is joined to `repo_root`. `Command` program resolution combined with
///   `current_dir` is platform-specific for relative program paths, so we anchor
///   it to the repo root to make the spawn unambiguous — matching the old exec
///   runtime, which resolved repo-relative executables against the root.
fn resolve_path_binding(repo_root: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let has_separator = path.contains('/') || path.contains('\\');
    if candidate.is_absolute() || !has_separator {
        PathBuf::from(path)
    } else {
        repo_root.join(candidate)
    }
}

/// Returns the trimmed first line of `binary --version`, or an empty string on failure.
pub fn binary_version_string(binary: &Path) -> String {
    Command::new(binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.lines().next().unwrap_or("").trim().to_owned())
        .unwrap_or_default()
}

/// Read an optional `applies_to` glob list override from the per-check `config` blob.
///
/// When `applies_to` is present in `config`, its value REPLACES the definition's
/// `applies_to` list entirely. The override uses the same glob vocabulary as the
/// check definition — a list of one or more glob strings. An empty list is rejected:
/// to disable a check entirely, use `enabled: false` instead.
///
/// Returns `None` when no override is present (caller uses the definition's list).
/// Returns `Some(Err(...))` on a malformed override.
/// Returns `Some(Ok(globs))` with the replacement list when the override is valid.
pub fn override_applies_to(config: &toml::Value) -> Option<Result<Vec<String>>> {
    let value = config.get("applies_to")?;
    let array = match value.as_array() {
        Some(arr) => arr,
        None => {
            return Some(Err(anyhow::anyhow!(
                "`applies_to` config override must be a list of glob strings, not a scalar"
            )));
        }
    };
    if array.is_empty() {
        return Some(Err(anyhow::anyhow!(
            "`applies_to` config override must not be empty; \
             use `enabled: false` to disable the check instead"
        )));
    }
    let mut globs = Vec::with_capacity(array.len());
    for (i, entry) in array.iter().enumerate() {
        match entry.as_str() {
            Some(s) if !s.trim().is_empty() => globs.push(s.to_owned()),
            Some(_) => {
                return Some(Err(anyhow::anyhow!(
                    "`applies_to[{i}]` config override entry must not be empty"
                )));
            }
            None => {
                return Some(Err(anyhow::anyhow!(
                    "`applies_to[{i}]` config override entry must be a string"
                )));
            }
        }
    }
    Some(Ok(globs))
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
        (None, None) => Some(Err(anyhow::anyhow!("override for `{name}` must set `bazel` or `path`"))),
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

/// Builds `target` and resolves its executable path via `bazel cquery --output=starlark`.
/// Returns an absolute path to the built binary.
///
/// `pub(crate)` so tests can call it directly (gated e2e, parity).
pub(crate) fn resolve_bazel_target_executable(repo_root: &Path, target: &str) -> Result<PathBuf> {
    let build_output = Command::new("bazel")
        .arg("build")
        .arg("--color=no")
        .arg("--curses=no")
        .arg("--noshow_progress")
        .arg("--show_result=0")
        .arg("--ui_event_filters=-info")
        .arg(target)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to spawn `bazel build {target}`"))?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        bail!(
            "`bazel build {target}` failed (exit {}): {}",
            build_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let cquery_output = Command::new("bazel")
        .arg("cquery")
        .arg("--color=no")
        .arg("--curses=no")
        .arg("--noshow_progress")
        .arg(target)
        .arg("--output=starlark")
        .arg("--starlark:expr=target.files_to_run.executable.path if target.files_to_run.executable else ''")
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to spawn `bazel cquery {target}`"))?;

    if !cquery_output.status.success() {
        let stderr = String::from_utf8_lossy(&cquery_output.stderr);
        bail!(
            "`bazel cquery {target}` failed (exit {}): {}",
            cquery_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let raw = String::from_utf8_lossy(&cquery_output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();

    if raw.is_empty() {
        bail!(
            "bazel target `{target}` does not produce an executable \
             (cquery returned an empty path)"
        );
    }

    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_root.join(path))
    }
}
