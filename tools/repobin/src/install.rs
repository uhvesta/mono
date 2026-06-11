use std::collections::BTreeSet;
use std::env;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::app::RepobinError;
use crate::config::RepoConfig;
use crate::defaults::{DEFAULTS_FILE_NAME, DefaultsConfig, DefaultsTool, load_defaults_at, write_defaults};
use crate::shell::{ShellFragment, bin_dir_on_path, path_update_fragment};

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub bin_dir: PathBuf,
    pub installed_binary: PathBuf,
    pub installed_tools: Vec<String>,
    pub defaults_written: Option<PathBuf>,
    pub defaults_skipped: Option<String>,
    pub path_warning: Option<PathWarning>,
}

#[derive(Debug, Clone)]
pub struct PathWarning {
    pub bin_dir: PathBuf,
    pub fragment: ShellFragment,
}

pub fn install(
    current_executable: &Path,
    repo_config: &RepoConfig,
    bin_dir: &Path,
    path_var: Option<&std::ffi::OsStr>,
    shell_var: Option<&std::ffi::OsStr>,
    home_dir: Option<&Path>,
    write_defaults_enabled: bool,
) -> Result<InstallReport, RepobinError> {
    std::fs::create_dir_all(bin_dir).map_err(|source| RepobinError::CreateBinDir {
        path: bin_dir.to_path_buf(),
        source,
    })?;

    let installed_binary = install_binary(current_executable, bin_dir)?;

    let defaults_path = bin_dir.join(DEFAULTS_FILE_NAME);
    let existing_defaults = load_defaults_at(&defaults_path)?
        .map(|loaded| loaded.config)
        .unwrap_or_else(DefaultsConfig::empty);

    let (updated_defaults, defaults_written, defaults_skipped) = if write_defaults_enabled {
        match discover_remote_url(&repo_config.repo_root) {
            Some(repo_url) => {
                let merged = merge_defaults(&existing_defaults, repo_config, &repo_url);
                write_defaults(&defaults_path, &merged)?;
                (merged, Some(defaults_path.clone()), None)
            }
            None => (
                existing_defaults,
                None,
                Some(format!(
                    "could not determine origin remote URL for `{}`; default mode requires `git remote get-url origin` to succeed",
                    repo_config.repo_root.display()
                )),
            ),
        }
    } else {
        (existing_defaults, None, None)
    };

    let mut symlink_names: BTreeSet<String> = repo_config.config.tools.keys().cloned().collect();
    symlink_names.extend(updated_defaults.tools.keys().cloned());
    let mut installed_tools = Vec::with_capacity(symlink_names.len());
    for tool_name in &symlink_names {
        install_tool_link(bin_dir, tool_name)?;
        installed_tools.push(tool_name.clone());
    }

    let path_warning = if bin_dir_on_path(bin_dir, path_var) {
        None
    } else {
        Some(PathWarning {
            bin_dir: bin_dir.to_path_buf(),
            fragment: path_update_fragment(bin_dir, shell_var, home_dir),
        })
    };

    Ok(InstallReport {
        bin_dir: bin_dir.to_path_buf(),
        installed_binary,
        installed_tools,
        defaults_written,
        defaults_skipped,
        path_warning,
    })
}

fn merge_defaults(existing: &DefaultsConfig, repo_config: &RepoConfig, repo_url: &str) -> DefaultsConfig {
    let mut merged = existing.clone();
    for name in repo_config.config.tools.keys() {
        merged.tools.insert(
            name.clone(),
            DefaultsTool {
                repo: repo_url.to_string(),
                sha: None,
            },
        );
    }
    merged
}

fn discover_remote_url(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() { None } else { Some(raw) }
}

pub fn resolve_bin_dir(requested: Option<&Path>, cwd: &Path, home_dir: Option<&Path>) -> Result<PathBuf, RepobinError> {
    let path = if let Some(requested) = requested {
        expand_user_path(requested, home_dir)?
    } else {
        default_bin_dir(home_dir)?
    };

    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(cwd.join(path))
    }
}

fn default_bin_dir(home_dir: Option<&Path>) -> Result<PathBuf, RepobinError> {
    let Some(home_dir) = home_dir else {
        return Err(RepobinError::MissingHomeDirectory);
    };
    Ok(home_dir.join("bin"))
}

fn expand_user_path(path: &Path, home_dir: Option<&Path>) -> Result<PathBuf, RepobinError> {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return Ok(path.to_path_buf());
    };
    if first != Component::Normal("~".as_ref()) {
        return Ok(path.to_path_buf());
    }

    let Some(home_dir) = home_dir else {
        return Err(RepobinError::MissingHomeDirectory);
    };

    let mut expanded = home_dir.to_path_buf();
    for component in components {
        expanded.push(component.as_os_str());
    }
    Ok(expanded)
}

fn install_binary(current_executable: &Path, bin_dir: &Path) -> Result<PathBuf, RepobinError> {
    let destination = bin_dir.join("repobin");
    let temp_destination = temporary_path(bin_dir, ".repobin");

    std::fs::copy(current_executable, &temp_destination).map_err(|source| RepobinError::CopyInstalledBinary {
        from: current_executable.to_path_buf(),
        to: destination.clone(),
        source,
    })?;

    let permissions = std::fs::metadata(current_executable).map_err(|source| RepobinError::ReadInstalledBinary {
        path: current_executable.to_path_buf(),
        source,
    })?;
    std::fs::set_permissions(&temp_destination, permissions.permissions()).map_err(|source| {
        RepobinError::WriteInstalledBinary {
            path: temp_destination.clone(),
            source,
        }
    })?;

    std::fs::rename(&temp_destination, &destination).map_err(|source| RepobinError::WriteInstalledBinary {
        path: destination.clone(),
        source,
    })?;

    Ok(destination)
}

fn install_tool_link(bin_dir: &Path, tool_name: &str) -> Result<(), RepobinError> {
    let destination = bin_dir.join(tool_name);
    let temp_destination = temporary_path(bin_dir, &format!(".{tool_name}"));

    let _ = std::fs::remove_file(&temp_destination);
    std::os::unix::fs::symlink("repobin", &temp_destination).map_err(|source| RepobinError::CreateToolSymlink {
        path: destination.clone(),
        source,
    })?;

    std::fs::rename(&temp_destination, &destination).map_err(|source| RepobinError::CreateToolSymlink {
        path: destination.clone(),
        source,
    })?;

    Ok(())
}

fn temporary_path(bin_dir: &Path, prefix: &str) -> PathBuf {
    bin_dir.join(format!("{prefix}.{}.tmp", std::process::id()))
}

pub fn current_home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::config::{Config, RepoConfig, ToolConfig};

    use super::{current_home_dir, install, resolve_bin_dir};

    fn sample_repo_config(root: &std::path::Path) -> RepoConfig {
        RepoConfig {
            repo_root: root.to_path_buf(),
            config_path: root.join("REPOBIN.toml"),
            config: Config {
                version: 1,
                tools: std::collections::BTreeMap::from([
                    (
                        "boss".to_string(),
                        ToolConfig {
                            target: "//tools/boss/cli:boss".to_string(),
                        },
                    ),
                    (
                        "cube".to_string(),
                        ToolConfig {
                            target: "//tools/cube:cube".to_string(),
                        },
                    ),
                ]),
            },
        }
    }

    #[test]
    fn resolve_bin_dir_defaults_to_home_bin() {
        let cwd = Path::new("/repo");
        let resolved = resolve_bin_dir(None, cwd, Some(Path::new("/Users/test"))).expect("bin dir");
        assert_eq!(resolved, Path::new("/Users/test/bin"));
    }

    #[test]
    fn resolve_bin_dir_expands_tilde_and_relative_paths() {
        let cwd = Path::new("/repo");
        let tilde =
            resolve_bin_dir(Some(Path::new("~/custom/bin")), cwd, Some(Path::new("/Users/test"))).expect("tilde path");
        assert_eq!(tilde, Path::new("/Users/test/custom/bin"));

        let relative =
            resolve_bin_dir(Some(Path::new(".bin")), cwd, Some(Path::new("/Users/test"))).expect("relative path");
        assert_eq!(relative, Path::new("/repo/.bin"));
    }

    #[test]
    fn install_copies_binary_and_creates_tool_links() {
        let temp = TempDir::new().expect("tempdir");
        let repo = sample_repo_config(temp.path());
        let source_binary = temp.path().join("repobin-source");
        fs::write(&source_binary, b"#!/bin/sh\nexit 0\n").expect("write source binary");
        fs::set_permissions(&source_binary, fs::Permissions::from_mode(0o755)).expect("chmod source binary");

        let bin_dir = temp.path().join("bin");
        let path_var = OsString::from("/usr/bin");
        let shell_var = OsString::from("/bin/zsh");
        let report = install(
            &source_binary,
            &repo,
            &bin_dir,
            Some(path_var.as_os_str()),
            Some(shell_var.as_os_str()),
            Some(Path::new("/Users/test")),
            false,
        )
        .expect("install");

        assert_eq!(report.installed_tools, vec!["boss".to_string(), "cube".to_string()]);
        assert_eq!(
            fs::read(bin_dir.join("repobin")).expect("read installed binary"),
            b"#!/bin/sh\nexit 0\n"
        );
        assert_eq!(
            std::fs::read_link(bin_dir.join("boss")).expect("boss symlink"),
            Path::new("repobin")
        );
        assert!(report.path_warning.is_some());
        assert!(report.defaults_written.is_none());
        assert!(report.defaults_skipped.is_none());
    }

    #[test]
    fn install_includes_defaults_only_tools_in_symlinks() {
        use crate::defaults::{DEFAULTS_FILE_NAME, DefaultsConfig, DefaultsTool, write_defaults};

        let temp = TempDir::new().expect("tempdir");
        let repo = sample_repo_config(temp.path());
        let source_binary = temp.path().join("repobin-source");
        fs::write(&source_binary, b"#!/bin/sh\nexit 0\n").expect("write source binary");
        fs::set_permissions(&source_binary, fs::Permissions::from_mode(0o755)).expect("chmod source binary");

        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");

        let mut tools = std::collections::BTreeMap::new();
        tools.insert(
            "leftover".to_string(),
            DefaultsTool {
                repo: "https://example.com/other.git".to_string(),
                sha: None,
            },
        );
        let preexisting = DefaultsConfig { version: 1, tools };
        write_defaults(&bin_dir.join(DEFAULTS_FILE_NAME), &preexisting).expect("write defaults");

        let report = install(
            &source_binary,
            &repo,
            &bin_dir,
            Some(OsString::from("/usr/bin").as_os_str()),
            Some(OsString::from("/bin/zsh").as_os_str()),
            Some(Path::new("/Users/test")),
            false,
        )
        .expect("install");

        assert!(report.installed_tools.contains(&"boss".to_string()));
        assert!(report.installed_tools.contains(&"cube".to_string()));
        assert!(report.installed_tools.contains(&"leftover".to_string()));
        assert_eq!(
            std::fs::read_link(bin_dir.join("leftover")).expect("leftover symlink"),
            Path::new("repobin")
        );
    }

    #[test]
    fn current_home_dir_matches_environment_when_present() {
        if let Some(home) = current_home_dir() {
            assert!(!home.as_os_str().is_empty());
        }
    }
}
