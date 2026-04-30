use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "repobin",
    about = "Install and dispatch repo-local Bazel tools"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install repobin and repo-configured tool symlinks into a user bin directory.
    Install(InstallArgs),
    /// Inspect the current repo's repobin configuration.
    Doctor(DoctorArgs),
    /// List configured tool names for the current repo.
    List,
    /// Execute a configured tool without relying on a symlink.
    Exec(ExecArgs),
}

#[derive(Debug, Clone, Args)]
pub struct BinDirArgs {
    /// Directory that should receive the installed `repobin` binary and tool links.
    #[arg(long, value_name = "DIR")]
    pub bin_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub bin_dir: BinDirArgs,

    /// Skip writing defaults to `repobin.yaml` next to the installed binary.
    #[arg(long)]
    pub no_defaults: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub bin_dir: BinDirArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ExecArgs {
    /// Tool name to resolve from REPOBIN.toml.
    pub tool: String,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn install_accepts_bin_dir_override() {
        let cli = Cli::parse_from(["repobin", "install", "--bin-dir", "~/.local/bin"]);

        match cli.command {
            Command::Install(args) => {
                assert_eq!(
                    args.bin_dir.bin_dir.as_deref(),
                    Some(std::path::Path::new("~/.local/bin"))
                );
            }
            _ => panic!("expected install command"),
        }
    }

    #[test]
    fn exec_preserves_trailing_args() {
        let cli = Cli::parse_from(["repobin", "exec", "boss", "--", "task", "list", "--json"]);

        match cli.command {
            Command::Exec(args) => {
                assert_eq!(args.tool, "boss");
                assert_eq!(
                    args.args,
                    vec![
                        OsString::from("task"),
                        OsString::from("list"),
                        OsString::from("--json"),
                    ]
                );
            }
            _ => panic!("expected exec command"),
        }
    }
}
