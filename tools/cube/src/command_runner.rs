use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::CubeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

pub trait CommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError>;

    /// Run a command with an upper bound on its wall-clock time. On expiry the
    /// child process is killed and reaped, and [`CubeError::CommandTimedOut`]
    /// is returned, so cube never blocks indefinitely on a wedged subprocess
    /// (e.g. a `jj git fetch` whose ssh connection went half-open).
    ///
    /// The default implementation ignores the timeout and delegates to
    /// [`CommandRunner::run`]; test fakes complete instantly, so they need no
    /// timeout handling. [`RealCommandRunner`] overrides this to enforce the
    /// bound against a real child process.
    fn run_with_timeout(&self, invocation: &CommandInvocation, _timeout: Duration) -> Result<String, CubeError> {
        self.run(invocation)
    }
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

    /// Apply the environment cube wants on every subprocess it spawns.
    fn configure_env(cmd: &mut Command) {
        // When cube's own stdout is not a terminal (e.g. piped by worker
        // automation), tell subprocesses to suppress ANSI colour codes and
        // interactive chrome. NO_COLOR is the cross-ecosystem standard; both
        // jj and gh honour it.
        if !std::io::stdout().is_terminal() {
            cmd.env("NO_COLOR", "1");
        }

        // Make a dead ssh connection fail in seconds instead of hanging on a
        // half-open TCP socket — the failure mode that wedged the per-repo
        // lock for 16+ minutes. `git` honours GIT_SSH_COMMAND directly; for
        // jj's own transport this is best-effort, with the run_with_timeout
        // bound as the hard backstop. Respect an operator-supplied override.
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env(
                "GIT_SSH_COMMAND",
                "ssh -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3",
            );
        }

        // Suppress git credential prompts unconditionally. cube runs headless
        // and has no terminal to type into; a credential prompt would hang the
        // subprocess until run_with_timeout kills it. Setting this to "0"
        // makes git fail immediately with an auth error instead of hanging.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&invocation.args).current_dir(&invocation.cwd);
        Self::configure_env(&mut cmd);

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

    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String, CubeError> {
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&invocation.args)
            .current_dir(&invocation.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::configure_env(&mut cmd);

        let mut child = cmd.spawn().map_err(CubeError::Io)?;

        // Drain stdout/stderr on dedicated threads so a child that produces
        // more output than the pipe buffer holds cannot deadlock against our
        // timeout poll (matching `Command::output`'s full-drain semantics).
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let out_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stdout_pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let err_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr_pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });

        let deadline = Instant::now() + timeout;
        // Adaptive backoff: fast commands return promptly, slow ones poll
        // cheaply. Capped so a hung command is detected within ~50ms of the
        // deadline.
        let mut wait = Duration::from_millis(1);
        let max_wait = Duration::from_millis(50);
        let status = loop {
            match child.try_wait().map_err(CubeError::Io)? {
                Some(status) => break status,
                None => {
                    if Instant::now() >= deadline {
                        // Kill the child so cube stops waiting and any lock it
                        // holds is released; reap it to avoid a zombie, and
                        // join the drain threads so the pipe fds close.
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = out_handle.join();
                        let _ = err_handle.join();
                        return Err(CubeError::CommandTimedOut {
                            program: invocation.program.clone(),
                            args: invocation.args.clone(),
                            timeout_secs: timeout.as_secs(),
                        });
                    }
                    thread::sleep(wait);
                    wait = (wait * 2).min(max_wait);
                }
            }
        };

        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();

        if status.success() {
            Ok(String::from_utf8_lossy(&stdout).trim().to_string())
        } else {
            Err(CubeError::CommandFailed {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                status: status.code(),
                stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_timeout_returns_output_for_fast_command() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let out = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "echo", &["hello"]),
                Duration::from_secs(10),
            )
            .expect("fast command should succeed");
        assert_eq!(out, "hello");
    }

    #[test]
    fn run_with_timeout_kills_and_errors_on_slow_command() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let started = Instant::now();
        let err = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "sleep", &["30"]),
                Duration::from_millis(200),
            )
            .expect_err("slow command should time out");
        let elapsed = started.elapsed();
        assert!(
            matches!(err, CubeError::CommandTimedOut { .. }),
            "expected CommandTimedOut, got {err:?}"
        );
        // Must return shortly after the deadline, not after the full sleep.
        assert!(
            elapsed < Duration::from_secs(5),
            "run_with_timeout returned in {elapsed:?}, expected to bail out near the 200ms deadline"
        );
    }

    #[test]
    fn run_with_timeout_propagates_nonzero_exit() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let err = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "false", &[]),
                Duration::from_secs(10),
            )
            .expect_err("`false` exits non-zero");
        assert!(
            matches!(err, CubeError::CommandFailed { .. }),
            "expected CommandFailed, got {err:?}"
        );
    }
}
