use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;
use thiserror::Error;

use crate::bazel::RealBazel;
use crate::cache::{EnsureOutcome, RepoCache, cache_root_from_env};
use crate::cli::{Cli, Command as CliCommand};
use crate::config::{CONFIG_FILE_NAME, load_repo_config};
use crate::defaults::{DEFAULTS_FILE_NAME, load_defaults_at, load_defaults_for_exe};
use crate::dispatch::{DispatchPlan, prepare_dispatch, prepare_dispatch_from_repo_config};
use crate::install::{InstallReport, current_home_dir, install, resolve_bin_dir};

const REPOBIN_BINARY_NAME: &str = "repobin";

#[derive(Debug, Error)]
pub enum RepobinError {
    #[error("no {CONFIG_FILE_NAME} found from `{}` upward", start_dir.display())]
    ConfigNotFound { start_dir: PathBuf },
    #[error("failed to read config `{}`", path.display())]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config `{}`", path.display())]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("unsupported {CONFIG_FILE_NAME} version `{version}`")]
    UnsupportedConfigVersion { version: u32 },
    #[error("{0}")]
    InvalidConfig(String),
    #[error("tool `{tool}` is not configured in `{}`", config_path.display())]
    ToolNotConfigured { tool: String, config_path: PathBuf },
    #[error(
        "tool `{tool}` is not configured locally and no default repo is set in `{}`",
        defaults_path.display()
    )]
    ToolNotConfiguredAnywhere {
        tool: String,
        defaults_path: PathBuf,
    },
    #[error("failed to read defaults file `{}`", path.display())]
    ReadDefaults {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse defaults file `{}`", path.display())]
    ParseDefaults {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to serialize defaults file `{}`", path.display())]
    SerializeDefaults {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to write defaults file `{}`", path.display())]
    WriteDefaults {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported {DEFAULTS_FILE_NAME} version `{version}`")]
    UnsupportedDefaultsVersion { version: u32 },
    #[error("{0}")]
    InvalidDefaults(String),
    #[error("HOME is not set and no --bin-dir override was provided")]
    MissingHomeDirectory,
    #[error("failed to create bin directory `{}`", path.display())]
    CreateBinDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read repobin binary `{}`", path.display())]
    ReadInstalledBinary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to copy repobin binary from `{}` to `{}`", from.display(), to.display())]
    CopyInstalledBinary {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write installed repobin binary `{}`", path.display())]
    WriteInstalledBinary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create tool symlink `{}`", path.display())]
    CreateToolSymlink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start bazel {action}")]
    SpawnBazel {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed while waiting for bazel {action}")]
    WaitBazel {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed while reading bazel {action} output")]
    ReadBazelOutput {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "bazel build failed for `{target}`{}",
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default()
    )]
    BazelBuildFailed { target: String, status: Option<i32> },
    #[error("failed to resolve executable path for `{target}`: {stderr}")]
    BazelQueryFailed { target: String, stderr: String },
    #[error("configured target `{target}` is not executable")]
    TargetNotExecutable { target: String },
    #[error("failed to start git {action}")]
    SpawnGit {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "git {action} failed{}",
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default()
    )]
    GitFailed { action: String, status: Option<i32> },
    #[error("failed to create cache directory `{}`", path.display())]
    CreateCacheDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open cache lock `{}`", path.display())]
    OpenCacheLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to acquire cache lock `{}`", path.display())]
    AcquireCacheLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write cache metadata `{}`", path.display())]
    WriteCacheMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to exec `{}`", path.display())]
    ExecTool {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RepobinError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::ConfigNotFound { .. }
            | Self::ParseConfig { .. }
            | Self::UnsupportedConfigVersion { .. }
            | Self::InvalidConfig(_)
            | Self::ToolNotConfigured { .. }
            | Self::ToolNotConfiguredAnywhere { .. }
            | Self::ParseDefaults { .. }
            | Self::UnsupportedDefaultsVersion { .. }
            | Self::InvalidDefaults(_)
            | Self::MissingHomeDirectory => ExitCode::from(2),
            _ => ExitCode::FAILURE,
        }
    }

    fn allows_default_fallback(&self) -> bool {
        matches!(
            self,
            Self::ConfigNotFound { .. } | Self::ToolNotConfigured { .. }
        )
    }
}

pub fn run_from_env() -> Result<ExitCode, RepobinError> {
    let args = env::args_os().collect::<Vec<_>>();
    let argv0 = args
        .first()
        .cloned()
        .unwrap_or_else(|| OsString::from(REPOBIN_BINARY_NAME));
    let invocation_name = invocation_name(&argv0);
    let cwd = env::current_dir()?;
    let current_executable = env::current_exe()?;

    if invocation_name != REPOBIN_BINARY_NAME {
        let forwarded_args = args.get(1..).unwrap_or(&[]).to_vec();
        dispatch_tool(&cwd, &current_executable, &invocation_name, &forwarded_args)?;
        return Ok(ExitCode::SUCCESS);
    }

    let cli = Cli::parse_from(args);
    run_cli(&cwd, &current_executable, cli)
}

fn run_cli(cwd: &Path, current_executable: &Path, cli: Cli) -> Result<ExitCode, RepobinError> {
    match cli.command {
        CliCommand::Install(args) => {
            let repo_config = load_repo_config(cwd)?;
            let home_dir = current_home_dir();
            let bin_dir =
                resolve_bin_dir(args.bin_dir.bin_dir.as_deref(), cwd, home_dir.as_deref())?;
            let report = install(
                current_executable,
                &repo_config,
                &bin_dir,
                env::var_os("PATH").as_deref(),
                env::var_os("SHELL").as_deref(),
                home_dir.as_deref(),
                !args.no_defaults,
            )?;

            print_install_report(&report);
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Doctor(args) => {
            let repo_config = load_repo_config(cwd)?;
            let home_dir = current_home_dir();
            let bin_dir =
                resolve_bin_dir(args.bin_dir.bin_dir.as_deref(), cwd, home_dir.as_deref())?;
            let on_path = crate::shell::bin_dir_on_path(&bin_dir, env::var_os("PATH").as_deref());

            println!("Repo root: {}", repo_config.repo_root.display());
            println!("Config: {}", repo_config.config_path.display());
            println!("Version: {}", repo_config.config.version);
            println!("Bin dir: {}", bin_dir.display());
            println!("On PATH: {}", if on_path { "yes" } else { "no" });
            println!("Tools:");
            for (name, tool) in &repo_config.config.tools {
                println!("  {name} -> {}", tool.target);
            }

            let defaults_file = bin_dir.join(DEFAULTS_FILE_NAME);
            match load_defaults_at(&defaults_file)? {
                Some(loaded) if !loaded.config.tools.is_empty() => {
                    println!("Defaults: {}", loaded.path.display());
                    for (name, tool) in &loaded.config.tools {
                        println!("  {name} -> {}", tool.repo);
                    }
                }
                Some(_) => {
                    println!("Defaults: {} (empty)", defaults_file.display());
                }
                None => {
                    println!("Defaults: (not configured)");
                }
            }

            if !on_path {
                let fragment = crate::shell::path_update_fragment(
                    &bin_dir,
                    env::var_os("SHELL").as_deref(),
                    home_dir.as_deref(),
                );
                println!("Suggested PATH fragment:");
                println!("{}", fragment.fragment);
            }

            Ok(ExitCode::SUCCESS)
        }
        CliCommand::List => {
            let repo_config = load_repo_config(cwd)?;
            for (name, tool) in &repo_config.config.tools {
                println!("{name} -> {}", tool.target);
            }
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Exec(args) => {
            dispatch_tool(cwd, current_executable, &args.tool, &args.args)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_install_report(report: &InstallReport) {
    println!("Installed repobin to {}", report.installed_binary.display());
    for tool in &report.installed_tools {
        println!(
            "Installed {} -> repobin",
            report.bin_dir.join(tool).display()
        );
    }
    if let Some(path) = &report.defaults_written {
        println!("Updated defaults at {}", path.display());
    }
    if let Some(notice) = &report.defaults_skipped {
        eprintln!("Skipped defaults update: {notice}");
    }

    if let Some(warning) = &report.path_warning {
        eprintln!("warning: `{}` is not on PATH", warning.bin_dir.display());
        if let Some(config_hint) = &warning.fragment.config_hint {
            eprintln!("Add this to {config_hint}:");
        } else {
            eprintln!("Add this to your shell config:");
        }
        eprintln!();
        eprintln!("{}", warning.fragment.fragment);
    }
}

fn dispatch_tool(
    cwd: &Path,
    current_executable: &Path,
    tool_name: &str,
    forwarded_args: &[OsString],
) -> Result<(), RepobinError> {
    let bazel = RealBazel::new(env::var_os("REPOBIN_VERBOSE").is_some());
    let local_err = match prepare_dispatch(&bazel, cwd, tool_name, forwarded_args) {
        Ok(plan) => return exec_dispatch(plan),
        Err(error) => error,
    };

    if !local_err.allows_default_fallback() {
        return Err(local_err);
    }

    let plan = match prepare_default_plan(
        &bazel,
        current_executable,
        cwd,
        tool_name,
        forwarded_args,
    )? {
        Some(plan) => plan,
        None => return Err(local_err),
    };
    exec_dispatch(plan)
}

fn prepare_default_plan<B: crate::bazel::BazelAdapter>(
    bazel: &B,
    current_executable: &Path,
    cwd: &Path,
    tool_name: &str,
    forwarded_args: &[OsString],
) -> Result<Option<DispatchPlan>, RepobinError> {
    let Some(loaded) = load_defaults_for_exe(current_executable)? else {
        return Ok(None);
    };
    let Some(tool) = loaded.config.tools.get(tool_name) else {
        return Err(RepobinError::ToolNotConfiguredAnywhere {
            tool: tool_name.to_string(),
            defaults_path: loaded.path,
        });
    };

    let cache_root = cache_root_from_env()?;
    let cache = RepoCache::for_url(&cache_root, &tool.repo);
    let lock = cache.lock()?;
    let outcome = lock.ensure_up_to_date()?;
    print_default_notice(tool_name, &tool.repo, &outcome, forwarded_args);

    let cached_repo_config = load_repo_config(&lock.cache().checkout)?;
    let plan = prepare_dispatch_from_repo_config(
        bazel,
        cached_repo_config,
        cwd,
        tool_name,
        forwarded_args,
    )?;
    Ok(Some(plan))
}

fn print_default_notice(
    tool_name: &str,
    repo: &str,
    outcome: &EnsureOutcome,
    forwarded_args: &[OsString],
) {
    if !should_emit_default_notice(outcome, forwarded_args, repobin_verbose()) {
        return;
    }
    let head = outcome.head();
    let short = if head.len() >= 7 { &head[..7] } else { head };
    eprintln!(
        "repobin: running `{tool_name}` from {repo} @ {short} ({}; default mode — not in a configured workspace)",
        outcome.note()
    );
}

fn repobin_verbose() -> bool {
    env::var_os("REPOBIN_VERBOSE").is_some()
}

fn args_request_json(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--json")
}

fn should_emit_default_notice(
    _outcome: &EnsureOutcome,
    forwarded_args: &[OsString],
    verbose: bool,
) -> bool {
    // --json is a strong signal the caller is parsing output; suppress on both
    // streams regardless of verbosity so `boss --json … 2>&1 | jq` is safe.
    if args_request_json(forwarded_args) {
        return false;
    }
    // Default mode + head + cached/updated/cloned is the routine case. Stay
    // silent unless the user asked for verbose output. Genuine errors (failed
    // clone, failed cache write) still surface via RepobinError on stderr.
    verbose
}

fn exec_dispatch(plan: DispatchPlan) -> Result<(), RepobinError> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(&plan.executable_path)
        .arg0(&plan.tool_name)
        .args(&plan.forwarded_args)
        .current_dir(&plan.original_cwd)
        .exec();
    Err(RepobinError::ExecTool {
        path: plan.executable_path,
        source: error,
    })
}

fn invocation_name(argv0: &OsString) -> String {
    Path::new(argv0)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| REPOBIN_BINARY_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::bazel::BazelAdapter;
    use crate::cache::EnsureOutcome;
    use crate::defaults::{DEFAULTS_FILE_NAME, DefaultsConfig, DefaultsTool, write_defaults};

    use super::{
        RepobinError, args_request_json, invocation_name, prepare_default_plan,
        should_emit_default_notice,
    };

    struct UnreachableBazel;

    impl BazelAdapter for UnreachableBazel {
        fn build(&self, _repo_root: &Path, _target: &str) -> Result<(), RepobinError> {
            panic!("bazel build should not be invoked in this test")
        }

        fn resolve_executable(
            &self,
            _repo_root: &Path,
            _target: &str,
        ) -> Result<PathBuf, RepobinError> {
            panic!("bazel cquery should not be invoked in this test")
        }

        fn resolve_source_files(
            &self,
            _repo_root: &Path,
            _target: &str,
        ) -> Result<Vec<PathBuf>, RepobinError> {
            panic!("bazel query should not be invoked in this test")
        }
    }

    #[test]
    fn invocation_name_uses_basename() {
        assert_eq!(
            invocation_name(&OsString::from("/Users/test/bin/boss")),
            "boss"
        );
        assert_eq!(invocation_name(&OsString::from("repobin")), "repobin");
        assert_eq!(
            invocation_name(&OsString::from(Path::new("").as_os_str())),
            "repobin"
        );
    }

    #[test]
    fn prepare_default_plan_returns_none_when_yaml_missing() {
        let temp = TempDir::new().unwrap();
        let exe = temp.path().join("repobin");
        let plan = prepare_default_plan(
            &UnreachableBazel,
            &exe,
            temp.path(),
            "boss",
            &[],
        )
        .expect("returns Ok");
        assert!(plan.is_none());
    }

    #[test]
    fn args_request_json_detects_flag() {
        assert!(args_request_json(&[OsString::from("--json")]));
        assert!(args_request_json(&[
            OsString::from("product"),
            OsString::from("list"),
            OsString::from("--json"),
        ]));
        assert!(!args_request_json(&[]));
        assert!(!args_request_json(&[OsString::from("--jsonish")]));
        assert!(!args_request_json(&[OsString::from("product"), OsString::from("list")]));
    }

    #[test]
    fn default_notice_is_silent_in_routine_case() {
        let outcomes = [
            EnsureOutcome::Cloned {
                head: "deadbeef".into(),
            },
            EnsureOutcome::Updated {
                head: "deadbeef".into(),
            },
            EnsureOutcome::Cached {
                head: "deadbeef".into(),
                refreshed: true,
            },
            EnsureOutcome::Cached {
                head: "deadbeef".into(),
                refreshed: false,
            },
        ];
        for outcome in &outcomes {
            assert!(
                !should_emit_default_notice(outcome, &[], false),
                "routine head/default-mode dispatch must stay silent: {outcome:?}"
            );
        }
    }

    #[test]
    fn default_notice_emits_when_verbose() {
        let outcome = EnsureOutcome::Cached {
            head: "deadbeef".into(),
            refreshed: false,
        };
        assert!(should_emit_default_notice(&outcome, &[], true));
    }

    #[test]
    fn json_args_silence_notice_even_when_verbose() {
        let outcome = EnsureOutcome::Updated {
            head: "deadbeef".into(),
        };
        let args = [OsString::from("product"), OsString::from("--json")];
        assert!(!should_emit_default_notice(&outcome, &args, true));
        assert!(!should_emit_default_notice(&outcome, &args, false));
    }

    #[test]
    fn prepare_default_plan_errors_when_tool_missing_from_defaults() {
        let temp = TempDir::new().unwrap();
        let exe = temp.path().join("repobin");

        let mut tools = BTreeMap::new();
        tools.insert(
            "cube".to_string(),
            DefaultsTool {
                repo: "https://example.com/x.git".to_string(),
            },
        );
        write_defaults(
            &temp.path().join(DEFAULTS_FILE_NAME),
            &DefaultsConfig { version: 1, tools },
        )
        .unwrap();

        let err = prepare_default_plan(
            &UnreachableBazel,
            &exe,
            temp.path(),
            "boss",
            &[],
        )
        .expect_err("expected ToolNotConfiguredAnywhere");
        match err {
            RepobinError::ToolNotConfiguredAnywhere { tool, defaults_path } => {
                assert_eq!(tool, "boss");
                assert_eq!(defaults_path, temp.path().join(DEFAULTS_FILE_NAME));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
