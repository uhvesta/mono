use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app::CubeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

pub trait CommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError>;
}

pub struct RealCommandRunner;

impl RealCommandRunner {
    pub fn invocation(cwd: &Path, program: &str, args: &[&str]) -> CommandInvocation {
        CommandInvocation {
            cwd: cwd.to_path_buf(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&invocation.args).current_dir(&invocation.cwd);

        // When cube's own stdout is not a terminal (e.g. piped by worker automation),
        // tell subprocesses to suppress ANSI colour codes and interactive chrome.
        // NO_COLOR is the cross-ecosystem standard; both jj and gh honour it.
        if !std::io::stdout().is_terminal() {
            cmd.env("NO_COLOR", "1");
        }

        let output = cmd.output().map_err(CubeError::Io)?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(CubeError::CommandFailed {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}
