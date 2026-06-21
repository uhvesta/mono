//! Framework-owned binary resolution.
//!
//! A declared binary requirement is resolved to a concrete command by the
//! *framework*, not by any guest code. Three resolvers are supported:
//!
//! - [`BinaryBinding::Path`] — a direct path or PATH name, used as-is. The
//!   portable fallback: standalone checkleft (no Bazel workspace) always has this.
//! - [`BinaryBinding::Bazel`] — a Bazel label, built then resolved to its
//!   executable. **Environment-conditional**: it requires a Bazel workspace, so it
//!   works in-repo but not in standalone checkleft. It reuses the *same* resolver
//!   the built-in buildifier check uses ([`resolve_bazel_target_executable`]),
//!   which proves the framework can own what the built-in hand-rolled.
//! - [`BinaryBinding::Npm`] — a version-pinned npm package, resolved to
//!   `npx --yes <package>@<version>`. **Environment-conditional**: it requires
//!   `npx`/Node on PATH. The version pin rides ahead of the check's own args as a
//!   [`ResolvedBinary::prefix_args`] entry, so npx runs exactly the pinned release.
//!
//! A CHECKS-config override may substitute a different binding per declared name,
//! keeping CHECKS.yaml thin (enable + repo-specific overrides only) while the
//! definition lives in the package manifest. An `npm` override may set only the
//! field it wants to change (e.g. just `version`); omitted fields inherit from the
//! default `npm` binding.
//!
//! When a `bazel`/`npm` binding is declared alongside a `fallback.path`, primary
//! resolution failure is non-fatal: the framework logs a loud warning to stderr
//! naming the reason and the resolved fallback binary, then continues. Without a
//! fallback declared, failure is an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{BinaryBinding, BinaryRequirement};

/// A declared binary resolved to a concrete command: the program to spawn plus any
/// prefix args that must precede the invocation's own templated args.
///
/// `path`/`bazel` bindings resolve to a bare program (`prefix_args` empty). The
/// `npm` binding resolves to `npx` with `["--yes", "<package>@<version>"]` as
/// prefix args, so the version pin precedes the check's own arguments.
///
/// `display_invocation` is the human-readable form used in remediation templates
/// via `{{needs.<name>.invocation}}`. For `npm` this is `"npx --yes <pkg>@<ver>"`
/// (not the full npx path); for `path`/`bazel` it is the program string itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinary {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
    pub display_invocation: String,
}

impl ResolvedBinary {
    /// A bare program with no prefix args (the common `path`/`bazel` case).
    fn bare(program: PathBuf) -> Self {
        let display_invocation = program.to_string_lossy().into_owned();
        Self {
            program,
            prefix_args: Vec::new(),
            display_invocation,
        }
    }
}

/// Resolve every declared binary to a concrete command, honoring CHECKS-config
/// overrides. Returns a map keyed by the declared name.
pub fn resolve_all(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    resolve_all_with_npx(repo_root, needs, config, locate_npx().as_deref())
}

/// Inner resolver with the located `npx` (or `None`) injected. Split out so tests
/// can drive both the resolved-pin and missing-npx-fallback paths deterministically
/// without depending on the host PATH.
pub(crate) fn resolve_all_with_npx(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
    npx: Option<&Path>,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    let mut resolved = BTreeMap::new();
    for (name, requirement) in needs {
        let override_binding = override_binding(name, requirement, config)
            .transpose()
            .with_context(|| format!("invalid config override for binary `{name}`"))?;

        let binary = if let Some(binding) = override_binding {
            resolve_binding(repo_root, &binding, npx)
                .with_context(|| format!("failed to resolve declared binary `{name}`"))?
        } else {
            resolve_requirement(repo_root, name, requirement, npx)?
        };

        resolved.insert(name.clone(), binary);
    }
    Ok(resolved)
}

/// Resolve using the default binding, falling back if declared and the primary
/// (bazel/npm) binding fails to resolve. A `path` default always resolves, so it
/// never reaches the fallback branch.
fn resolve_requirement(
    repo_root: &Path,
    name: &str,
    requirement: &BinaryRequirement,
    npx: Option<&Path>,
) -> Result<ResolvedBinary> {
    match resolve_binding(repo_root, &requirement.default, npx) {
        Ok(binary) => Ok(binary),
        Err(err) => {
            let Some(fallback) = &requirement.fallback else {
                return Err(err).with_context(|| {
                    format!(
                        "failed to resolve declared binary `{name}`; \
                         declare `needs.{name}.fallback.path` for environments without the primary toolchain"
                    )
                });
            };
            let fallback_binary = resolve_binding(repo_root, fallback, npx).with_context(|| {
                format!("primary resolution of `{name}` failed AND fallback resolution also failed")
            })?;
            let version = binary_version_string(&fallback_binary.program);
            eprintln!(
                "warning: checkleft: `{name}`: hermetic toolchain unresolved ({err:#}); \
                 falling back to PATH binary `{}` ({})",
                fallback_binary.program.display(),
                if version.is_empty() {
                    "version unknown".to_owned()
                } else {
                    version
                }
            );
            Ok(fallback_binary)
        }
    }
}

fn resolve_binding(repo_root: &Path, binding: &BinaryBinding, npx: Option<&Path>) -> Result<ResolvedBinary> {
    match binding {
        BinaryBinding::Path(path) => Ok(ResolvedBinary::bare(resolve_path_binding(repo_root, path))),
        BinaryBinding::Bazel(target) => Ok(ResolvedBinary::bare(resolve_bazel_target_executable(
            repo_root, target,
        )?)),
        BinaryBinding::Npm { package, version } => resolve_npm_binding(package, version, npx),
    }
}

/// Resolve an `npm` binding to `npx --yes <package>@<version>`. The version is part
/// of the package spec, so npx fetches and runs exactly that release regardless of
/// any globally-installed copy. Returns `Err` (so a declared `fallback` can take
/// over) when `npx` is not on PATH.
fn resolve_npm_binding(package: &str, version: &str, npx: Option<&Path>) -> Result<ResolvedBinary> {
    let npx = npx.ok_or_else(|| {
        anyhow::anyhow!(
            "`npx` not found on PATH; cannot provision npm package `{package}@{version}` \
             (install Node.js/npm, or declare a `needs.<name>.fallback.path`)"
        )
    })?;
    Ok(ResolvedBinary {
        program: npx.to_path_buf(),
        // `--yes` auto-confirms fetching the pinned package non-interactively (CI);
        // the `<package>@<version>` spec forces the exact release.
        prefix_args: vec!["--yes".to_owned(), format!("{package}@{version}")],
        // Use a short "npx" name rather than the resolved absolute path so that
        // remediation strings like `npx --yes prettier@3.8.4 --write <file>` are
        // readable regardless of where npx is installed on the host.
        display_invocation: format!("npx --yes {package}@{version}"),
    })
}

/// Locate the `npx` launcher on PATH. Returns the first match, or `None` when npx
/// is not installed — which makes an `npm` binding fall back to its declared path.
fn locate_npx() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("npx");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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

/// Read an optional binding override from `config` at `needs.<name>.{path|bazel|npm}`.
///
/// An `npm` override may set only the field it wants to change; any omitted
/// `package`/`version` is inherited from the requirement's default `npm` binding,
/// so a repo can re-pin the version with just `needs.<name>.npm.version = "<X>"`.
fn override_binding(
    name: &str,
    requirement: &BinaryRequirement,
    config: &toml::Value,
) -> Option<Result<BinaryBinding>> {
    let entry = config.get("needs")?.get(name)?;
    let bazel = entry.get("bazel").and_then(toml::Value::as_str);
    let path = entry.get("path").and_then(toml::Value::as_str);
    let npm = entry.get("npm");

    let set_count = bazel.is_some() as u8 + path.is_some() as u8 + npm.is_some() as u8;
    if set_count == 0 {
        return Some(Err(anyhow::anyhow!(
            "override for `{name}` must set `bazel`, `path`, or `npm`"
        )));
    }
    if set_count > 1 {
        return Some(Err(anyhow::anyhow!(
            "override for `{name}` sets more than one of `bazel`/`path`/`npm`; set exactly one"
        )));
    }

    let result = if let Some(bazel) = bazel {
        Ok(BinaryBinding::Bazel(bazel.to_owned()))
    } else if let Some(path) = path {
        Ok(BinaryBinding::Path(path.to_owned()))
    } else {
        override_npm_binding(name, requirement, npm.expect("npm set when others unset"))
    };

    Some(result.and_then(|binding| match &binding {
        BinaryBinding::Path(value) | BinaryBinding::Bazel(value) if value.trim().is_empty() => {
            bail!("override for `{name}` must not be empty")
        }
        BinaryBinding::Npm { package, version } if package.trim().is_empty() || version.trim().is_empty() => {
            bail!("`npm` override for `{name}` must not have an empty `package` or `version`")
        }
        _ => Ok(binding),
    }))
}

/// Build an `npm` override binding, inheriting `package`/`version` from the
/// requirement's default `npm` binding for any field the override omits.
fn override_npm_binding(name: &str, requirement: &BinaryRequirement, npm: &toml::Value) -> Result<BinaryBinding> {
    let (default_package, default_version) = match &requirement.default {
        BinaryBinding::Npm { package, version } => (Some(package.as_str()), Some(version.as_str())),
        _ => (None, None),
    };
    let package = npm.get("package").and_then(toml::Value::as_str).or(default_package);
    let version = npm.get("version").and_then(toml::Value::as_str).or(default_version);
    match (package, version) {
        (Some(package), Some(version)) => Ok(BinaryBinding::Npm {
            package: package.to_owned(),
            version: version.to_owned(),
        }),
        _ => bail!(
            "`npm` override for `{name}` must set `package` and `version` \
             (or omit them to inherit from a default `npm` binding)"
        ),
    }
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
