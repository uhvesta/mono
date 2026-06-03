//! SSH transport for the `SshHostAdapter`.
//!
//! Owns the lifecycle of `OpenSSH ControlMaster` connections: opens a
//! persistent multiplexed connection per host, exposes helpers to run
//! commands and `scp` files through it, and sweeps stale control
//! sockets on engine startup.
//!
//! Socket policy: control sockets live under an engine-owned directory
//! (`$BOSS_RUNTIME_DIR/ssh/cm-<host_id>` or
//! `$HOME/.boss-remote-control/<host_id>` as fallback). The design's
//! "Risks and Open Questions" called out that placing sockets in
//! `~/.ssh` blurs the line between user-managed and engine-managed
//! state; the engine owns its sockets so a `bossctl hosts remove`
//! can scrub them without touching the user's own ssh state.
//!
//! Non-goals for Phase 3:
//!
//! - Reconnect on transient drops. The first failure surfaces as a run
//!   failure with reason `host_unreachable`; recovery happens by
//!   re-dispatching (Phase 6 handles classification, Phase 8 handles
//!   the cross-host retry policy).
//! - Probe/interrupt/stop on the control channel. Phase 4 lands those
//!   handlers; this phase only wires the transport.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::time::timeout;

/// Time budget for one `ssh` invocation. Long enough for cube to lease
/// a workspace (which can take a couple seconds on a cold cube pool)
/// but tight enough that an unreachable host fails fast.
pub const SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Time budget for the initial `ControlMaster` open. Includes the
/// network round-trip and ssh-config parsing; on a healthy LAN this
/// is well under a second.
pub const CONTROL_MASTER_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Time budget for one `scp` push. The wrapper script is small (a few
/// hundred bytes) so a longer-than-this push means the link is broken
/// or the remote disk is full; surface that as `connection_lost` or
/// `disk_full` per the Q6 design table.
pub const SCP_PUSH_TIMEOUT: Duration = Duration::from_secs(20);

/// Time budget for a control-channel command (`-O forward` / `-O cancel`)
/// issued against the existing master. These are local round-trips to
/// the master process (no new network handshake), so they complete in
/// well under a second on a healthy link.
pub const CONTROL_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Classified outcome from running an ssh-side command. Callers that
/// need fine-grained failure-mode mapping (e.g. wrapper push) inspect
/// the stderr-derived classification via [`classify_stderr`].
#[derive(Debug, Clone)]
pub struct SshOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl SshOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// One persistent ssh multiplex connection to a remote host. Created
/// at host-registration time or on first dispatch; reused for the
/// lifetime of the engine. Tearing one down explicitly is rare —
/// dropping the file is enough on engine shutdown because the OS
/// reaps the master process via SIGHUP when its controlling tty (the
/// engine) exits.
#[derive(Debug, Clone)]
pub struct SshTransport {
    /// Stable id used to name the control socket and in log lines.
    pub host_id: String,
    /// ssh-config alias or `user@host[:port]` — the literal argument
    /// passed to `ssh`.
    pub ssh_target: String,
    /// Path to the engine-owned control socket. Created when the
    /// master is opened; unlinked when [`close`] is called or on
    /// engine startup sweep.
    pub control_socket: PathBuf,
}

impl SshTransport {
    pub fn new(host_id: &str, ssh_target: &str, base_dir: &Path) -> Self {
        let control_socket = base_dir.join(format!("cm-{}.sock", sanitize_for_path(host_id)));
        Self {
            host_id: host_id.to_owned(),
            ssh_target: ssh_target.to_owned(),
            control_socket,
        }
    }

    /// Open the `ControlMaster`. Idempotent — if the socket already
    /// exists and is responsive (probed via `ssh -O check`) we treat
    /// the existing master as live; otherwise we unlink and re-open.
    pub async fn open_control_master(&self) -> Result<()> {
        if let Some(parent) = self.control_socket.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ssh-control parent dir {parent:?}"))?;
        }

        // Probe existing master first. `ssh -O check` returns 0 when
        // the master is alive; non-zero (typical EOF when the socket
        // file is dangling) is the trigger to reopen.
        if self.control_socket.exists() && self.check_control_master().await.unwrap_or(false) {
            tracing::debug!(
                host_id = %self.host_id,
                socket = %self.control_socket.display(),
                "ssh control master already alive; reusing"
            );
            return Ok(());
        }

        // Stale or missing — unlink so the master can re-bind.
        if self.control_socket.exists()
            && let Err(err) = std::fs::remove_file(&self.control_socket) {
                tracing::warn!(
                    ?err,
                    socket = %self.control_socket.display(),
                    "could not unlink stale ssh control socket"
                );
            }

        // Open a fresh master. `-M -N -f` is the canonical idiom:
        //   -M  this connection is the master
        //   -N  don't execute a remote command (just hold the master open)
        //   -f  background after auth
        // The connection lives until killed; the engine doesn't own
        // the PID directly but `ssh -O exit` (in [`close`]) tears it down.
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", "ServerAliveInterval=30",
            "-o", "ServerAliveCountMax=3",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            "-M", "-N", "-f",
            &self.ssh_target,
        ]);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning ssh master for host {}", self.host_id))?;
        let result = timeout(CONTROL_MASTER_OPEN_TIMEOUT, child.wait_with_output())
            .await
            .with_context(|| format!("ssh master open timed out for host {}", self.host_id))?
            .with_context(|| format!("ssh master open io error for host {}", self.host_id))?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
            bail!(
                "ssh master open failed for host {} (exit {}): {}",
                self.host_id,
                result.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        tracing::info!(
            host_id = %self.host_id,
            socket = %self.control_socket.display(),
            "ssh control master opened"
        );
        Ok(())
    }

    /// Probe the existing master with `ssh -O check`. Returns Ok(true)
    /// when the master process responds; Ok(false) when the socket is
    /// dangling (the typical case after engine restart). Errors from
    /// the ssh subprocess itself bubble up.
    pub async fn check_control_master(&self) -> Result<bool> {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            "-O", "check",
            &self.ssh_target,
        ]);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let status = cmd
            .status()
            .await
            .with_context(|| format!("ssh -O check failed to spawn for host {}", self.host_id))?;
        Ok(status.success())
    }

    /// Close the master gracefully via `ssh -O exit`. Best-effort;
    /// dangling sockets are cleaned up by [`sweep_stale_control_sockets`]
    /// on the next engine startup.
    pub async fn close(&self) -> Result<()> {
        if !self.control_socket.exists() {
            return Ok(());
        }
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            "-O", "exit",
            &self.ssh_target,
        ]);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let _ = cmd.status().await;
        Ok(())
    }

    /// Run a single command on the remote via the master connection.
    /// Captures stdout + stderr. Does not stream; callers that need a
    /// long-running worker stdio path (e.g. `spawn_worker`) build a
    /// fresh `tokio::process::Command` configured with the same
    /// `ControlPath` so they can capture pipes directly.
    pub async fn run(&self, argv: &[&str]) -> Result<SshOutput> {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            &self.ssh_target,
        ]);
        cmd.args(argv);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = timeout(SSH_COMMAND_TIMEOUT, cmd.output())
            .await
            .with_context(|| {
                format!("ssh run timed out for host {} cmd {:?}", self.host_id, argv)
            })?
            .with_context(|| format!("ssh run io error for host {}", self.host_id))?;

        Ok(SshOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// `scp` a local file to a remote path over the master connection.
    /// The remote path is interpreted by ssh's shell — `~` expansion
    /// works, but the caller should not pass shell metacharacters from
    /// untrusted sources. Host ids and `ssh_target` values come from
    /// the validated `hosts` table.
    pub async fn scp_push(&self, local: &Path, remote: &str) -> Result<SshOutput> {
        let mut cmd = Command::new("scp");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            local.to_string_lossy().as_ref(),
            &format!("{}:{remote}", self.ssh_target),
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = timeout(SCP_PUSH_TIMEOUT, cmd.output())
            .await
            .with_context(|| {
                format!("scp push timed out for host {} -> {remote}", self.host_id)
            })?
            .with_context(|| format!("scp push io error for host {}", self.host_id))?;

        Ok(SshOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Request a reverse Unix-socket forward over the existing master:
    /// connections to `remote_socket` on the remote are forwarded to
    /// `local_socket` on this (engine) host. Routed through
    /// `ssh -O forward` so the forward rides the persistent
    /// `ControlMaster` — it does **not** open a second SSH session, so
    /// the "one multiplex per host" invariant holds. The worker's
    /// `boss-event` shim then writes to what looks like a local socket
    /// at `remote_socket` and the bytes arrive on the engine's events
    /// socket.
    pub async fn add_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput> {
        self.control_forward("forward", remote_socket, local_socket).await
    }

    /// Tear down a forward previously requested with
    /// [`add_reverse_unix_forward`]. Best-effort: a failure here leaks a
    /// forward on the master until the master itself exits, which the
    /// startup sweep handles.
    pub async fn cancel_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput> {
        self.control_forward("cancel", remote_socket, local_socket).await
    }

    /// Shared body for `-O forward` / `-O cancel`. Both take the same
    /// `-R <remote>:<local>` forward spec and target the existing
    /// master via its `ControlPath`.
    async fn control_forward(
        &self,
        op: &str,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput> {
        let spec = reverse_forward_spec(remote_socket, local_socket);
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", &format!("ControlPath={}", self.control_socket.display()),
            "-O", op,
            "-R", &spec,
            &self.ssh_target,
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = timeout(CONTROL_COMMAND_TIMEOUT, cmd.output())
            .await
            .with_context(|| {
                format!("ssh -O {op} timed out for host {}", self.host_id)
            })?
            .with_context(|| format!("ssh -O {op} io error for host {}", self.host_id))?;

        Ok(SshOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Build the `-R` forward spec for a reverse Unix-socket forward:
/// `remote_socket_path:local_socket_path`. Kept as a free function so
/// the (purely syntactic) join is unit-testable without spawning ssh.
pub fn reverse_forward_spec(remote_socket: &str, local_socket: &str) -> String {
    format!("{remote_socket}:{local_socket}")
}

/// Classify an `scp` or `ssh` stderr blob into one of the Q6 wrapper-
/// push sub-classifications. Order matters: disk-full / permission /
/// connection-lost — fall through to `unclassified` so the engine
/// doesn't silently misattribute a novel failure mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshFailureKind {
    DiskFull,
    PermissionDenied,
    ConnectionLost,
    Unclassified,
}

pub fn classify_stderr(stderr: &str) -> SshFailureKind {
    let lower = stderr.to_lowercase();
    if lower.contains("no space left on device") || lower.contains("disk full") {
        return SshFailureKind::DiskFull;
    }
    if lower.contains("permission denied") {
        return SshFailureKind::PermissionDenied;
    }
    if lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("broken pipe")
        || lower.contains("operation timed out")
        || lower.contains("ssh: connect to host")
    {
        return SshFailureKind::ConnectionLost;
    }
    SshFailureKind::Unclassified
}

/// On engine startup, unlink any control sockets in the engine-owned
/// directory. Stale sockets from a previous run that crashed without
/// running [`SshTransport::close`] are the only thing that can prevent
/// a fresh master from binding cleanly. Per the design risks: this
/// sweep is non-negotiable.
pub fn sweep_stale_control_sockets(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {dir:?}"))? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.starts_with("cm-") || !name.ends_with(".sock") {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(?err, path = %path.display(), "sweep: failed to unlink stale control socket");
            continue;
        }
        count += 1;
    }
    if count > 0 {
        tracing::info!(count, dir = %dir.display(), "swept stale ssh control sockets at startup");
    }
    Ok(count)
}

/// Engine's preferred control-socket parent directory. Honors
/// `BOSS_RUNTIME_DIR` when set (test harness, headless CI) and
/// otherwise falls back to `$HOME/.boss-remote-control`.
pub fn default_control_socket_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BOSS_RUNTIME_DIR") {
        return Some(PathBuf::from(dir).join("ssh"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".boss-remote-control"))
}

/// `host_id` may carry characters that are legal in our id regex
/// (`[a-zA-Z0-9._@:-]+`) but ambiguous in a filename. Replace anything
/// outside `[a-zA-Z0-9_-]` with `_` so the socket path is unambiguous
/// regardless of platform.
fn sanitize_for_path(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn classify_disk_full() {
        let err = "scp: write: No space left on device\n";
        assert_eq!(classify_stderr(err), SshFailureKind::DiskFull);
    }

    #[test]
    fn classify_permission_denied() {
        let err = "scp: ~/.boss-remote/bin/boss-remote-run: Permission denied\n";
        assert_eq!(classify_stderr(err), SshFailureKind::PermissionDenied);
    }

    #[test]
    fn classify_connection_lost_variants() {
        for line in [
            "ssh: connect to host zakalwe port 22: Connection refused",
            "client_loop: send disconnect: Broken pipe",
            "ssh: connect to host zakalwe port 22: Operation timed out",
            "ssh: connect to host zakalwe port 22: No route to host",
            "Connection reset by 192.168.1.42 port 22",
        ] {
            assert_eq!(
                classify_stderr(line),
                SshFailureKind::ConnectionLost,
                "expected ConnectionLost for `{line}`"
            );
        }
    }

    #[test]
    fn classify_unclassified_falls_through() {
        assert_eq!(
            classify_stderr("something the engine has never seen before"),
            SshFailureKind::Unclassified,
        );
    }

    #[test]
    fn sweep_removes_cm_sockets_and_leaves_others() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("cm-zakalwe.sock"), b"stale").unwrap();
        std::fs::write(dir.path().join("cm-other.sock"), b"stale").unwrap();
        std::fs::write(dir.path().join("not-a-control-socket.txt"), b"keep").unwrap();
        let n = sweep_stale_control_sockets(dir.path()).unwrap();
        assert_eq!(n, 2);
        assert!(!dir.path().join("cm-zakalwe.sock").exists());
        assert!(!dir.path().join("cm-other.sock").exists());
        assert!(dir.path().join("not-a-control-socket.txt").exists());
    }

    #[test]
    fn sweep_returns_zero_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let n = sweep_stale_control_sockets(&nonexistent).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn transport_socket_path_is_under_base_dir() {
        let dir = TempDir::new().unwrap();
        let t = SshTransport::new("zakalwe", "user@zakalwe", dir.path());
        assert_eq!(t.host_id, "zakalwe");
        assert_eq!(t.ssh_target, "user@zakalwe");
        assert!(t.control_socket.starts_with(dir.path()));
        assert_eq!(
            t.control_socket.file_name().unwrap().to_str().unwrap(),
            "cm-zakalwe.sock"
        );
    }

    #[test]
    fn reverse_forward_spec_joins_remote_then_local() {
        // `-R <remote>:<local>` — the remote bind path comes first,
        // the engine-local target second. A swap here would forward
        // the wrong direction and silently drop every hook event.
        assert_eq!(
            reverse_forward_spec(
                "/tmp/boss-events-run-1.sock",
                "/Users/me/Library/Application Support/Boss/events.sock",
            ),
            "/tmp/boss-events-run-1.sock:/Users/me/Library/Application Support/Boss/events.sock",
        );
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        // The host-id validator at insert time permits `.`, `@`, `:`,
        // and `-`. None of those are unsafe in a Unix path component,
        // but `@`/`:` look weird in directory listings and `.` can
        // collide with hidden-file conventions in some shells. We
        // map only the truly-unsafe glyphs (none in the allowed set),
        // so a typical id round-trips unchanged.
        assert_eq!(sanitize_for_path("zakalwe"), "zakalwe");
        assert_eq!(sanitize_for_path("user-host_1"), "user-host_1");
        // Anything outside [A-Za-z0-9_-] (e.g. `@`, `:`, `.`) becomes `_`.
        assert_eq!(sanitize_for_path("user@zak.example"), "user_zak_example");
    }
}
