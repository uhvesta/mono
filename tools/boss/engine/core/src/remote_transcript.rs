//! Pull a remote worker's transcript tail over SSH.
//!
//! Remote-execution UX parity (dispatch-stack PR 4). A local worker's
//! transcript is a JSONL file the engine reads straight off its own
//! filesystem; a remote worker's transcript lives on the remote host.
//! The design's Q7 keeps "the surface identical to local" — so instead
//! of a new streaming protocol this module reads the remote file the
//! cheap way: an on-demand `tail -c <bytes>` over the host's existing
//! `ControlMaster`, returning the same JSONL bytes the local path would
//! have read. Callers split the result into lines exactly as they do for
//! a local transcript.
//!
//! Kept transport-agnostic via the [`SshExec`] seam so the whole pull is
//! exercised in-process against a stubbed transport — CI never depends on
//! a live remote.

use anyhow::{Result, anyhow};

use crate::ssh_spawn::SshExec;

/// Compose the remote command that reads the last `max_bytes` of the
/// transcript at `path`.
///
/// A single shell string (not a multi-token argv) so the remote shell
/// evaluates one well-formed command — the same convention
/// `SshHostAdapter::append_remote_bazel_gate` uses. `tail -c` reads a
/// byte suffix; `--` ends option parsing so a path that begins with `-`
/// is treated as a filename; and the path is single-quoted + escaped so
/// spaces or shell metacharacters in a cube/claude-produced path can
/// neither break the parse nor inject a command.
pub fn remote_tail_command(path: &str, max_bytes: u64) -> String {
    format!("tail -c {max_bytes} -- {}", shell_single_quote(path))
}

/// Single-quote `s` for a POSIX shell, escaping embedded single quotes
/// via the standard `'\''` idiom. The result is always safe to splice
/// into a command string as one argument.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Read up to `max_bytes` from the tail of the transcript at `path` on
/// the remote host reached by `exec`, returning the raw JSONL text.
///
/// A non-existent file is **not** an error: `tail` of a missing file
/// exits non-zero, but the caller wants the same "no transcript yet"
/// shape it gets locally, so an exit naming a missing file maps to an
/// empty string. Any other non-zero exit surfaces as an error so a real
/// failure (permission denied, connection lost) is not silently read as
/// an empty transcript.
pub async fn pull_remote_transcript_tail(
    exec: &dyn SshExec,
    path: &str,
    max_bytes: u64,
) -> Result<String> {
    let command = remote_tail_command(path, max_bytes);
    let out = exec.run(&[command.as_str()]).await?;
    if out.success() {
        return Ok(out.stdout);
    }
    let stderr_lower = out.stderr.to_lowercase();
    if stderr_lower.contains("no such file")
        || stderr_lower.contains("not found")
        || stderr_lower.contains("cannot open")
    {
        // The worker hasn't created the transcript yet (or it was
        // rotated away). Mirror the local "missing file → empty" path.
        return Ok(String::new());
    }
    let detail = if out.stderr.trim().is_empty() {
        format!("exit {}", out.status)
    } else {
        out.stderr.trim().to_owned()
    };
    Err(anyhow!(
        "remote transcript tail failed on host {}: {detail}",
        exec.host_id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_transport::SshOutput;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FakeExec {
        last_command: Mutex<Option<String>>,
        status: i32,
        stdout: String,
        stderr: String,
    }

    impl FakeExec {
        fn ok(stdout: &str) -> Self {
            Self {
                last_command: Mutex::new(None),
                status: 0,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            }
        }
        fn failing(status: i32, stderr: &str) -> Self {
            Self {
                last_command: Mutex::new(None),
                status,
                stdout: String::new(),
                stderr: stderr.to_owned(),
            }
        }
    }

    #[async_trait]
    impl SshExec for FakeExec {
        fn host_id(&self) -> &str {
            "zakalwe"
        }
        async fn run(&self, argv: &[&str]) -> Result<SshOutput> {
            *self.last_command.lock().unwrap() = argv.first().map(|s| s.to_string());
            Ok(SshOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
        async fn add_reverse_unix_forward(&self, _: &str, _: &str) -> Result<SshOutput> {
            unreachable!("transcript pull never forwards")
        }
        async fn cancel_reverse_unix_forward(&self, _: &str, _: &str) -> Result<SshOutput> {
            unreachable!("transcript pull never cancels")
        }
    }

    #[test]
    fn tail_command_byte_bounded_and_path_quoted() {
        assert_eq!(
            remote_tail_command("/Users/me/.claude/projects/abc/s.jsonl", 1024),
            "tail -c 1024 -- '/Users/me/.claude/projects/abc/s.jsonl'",
        );
    }

    #[test]
    fn tail_command_escapes_single_quote_and_metachars() {
        // A path with a single quote and a `$(...)` injection attempt is
        // neutralised: the whole path stays a single quoted argument.
        let cmd = remote_tail_command("/tmp/a'b/$(rm -rf ~).jsonl", 64);
        assert_eq!(cmd, "tail -c 64 -- '/tmp/a'\\''b/$(rm -rf ~).jsonl'");
    }

    #[tokio::test]
    async fn pull_returns_stdout_on_success() {
        let exec = FakeExec::ok("{\"a\":1}\n{\"b\":2}\n");
        let out = pull_remote_transcript_tail(&exec, "/p.jsonl", 4096)
            .await
            .unwrap();
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(
            exec.last_command.lock().unwrap().as_deref(),
            Some("tail -c 4096 -- '/p.jsonl'"),
        );
    }

    #[tokio::test]
    async fn pull_maps_missing_file_to_empty() {
        let exec = FakeExec::failing(1, "tail: /p.jsonl: No such file or directory");
        let out = pull_remote_transcript_tail(&exec, "/p.jsonl", 4096)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn pull_surfaces_real_failures() {
        let exec = FakeExec::failing(255, "ssh: connect to host zakalwe port 22: Broken pipe");
        let err = pull_remote_transcript_tail(&exec, "/p.jsonl", 4096)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("zakalwe"));
        assert!(err.to_string().contains("Broken pipe"));
    }
}
