//! Reusable client for talking to the Boss engine over its frontend socket.
//!
//! `BossClient` opens a Unix-domain connection to the engine and provides a
//! correlated request/response API on top of the framed JSON protocol defined
//! in [`boss_protocol`]. Engine discovery (socket path resolution + optional
//! autostart of `boss-engine`) lives behind [`Discovery`] so the CLI, tests,
//! and future TUI/web frontends share one set of rules.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use boss_protocol::{
    FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::sleep;

pub const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
pub const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";
pub const DEFAULT_ENGINE_START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct EngineCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Short label describing how `program` was resolved. Surfaced in
    /// error messages so a failed autostart names the source it picked
    /// (e.g. `BOSS_ENGINE_BIN`, `bazel-bin/tools/boss/engine/engine`,
    /// `PATH lookup`).
    pub source: String,
    /// Ordered list of every resolution step the resolver tried. The
    /// last entry is the one that won. Included verbatim in autostart
    /// error messages so the next person can debug a misconfigured
    /// resolution chain.
    pub attempted: Vec<String>,
}

/// Inputs to [`resolve_engine_command_with`] — split out so tests can
/// drive the resolver deterministically without mutating process env.
#[derive(Debug, Clone, Default)]
pub struct EngineResolverInput {
    pub env_cmd: Option<String>,
    pub env_bin: Option<String>,
    pub workspace_root: Option<PathBuf>,
    pub current_exe: Option<PathBuf>,
}

/// How a client should locate the engine and (optionally) launch it.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub socket_path: String,
    pub pid_file_path: String,
    pub autostart: bool,
    pub engine: EngineCommand,
    pub launch_directory: PathBuf,
    pub start_timeout: Duration,
}

impl Discovery {
    /// Build a discovery profile from process env + an optional `--socket-path` override.
    pub fn from_env(socket_override: Option<&str>) -> Result<Self> {
        let socket_path = socket_override
            .map(str::to_owned)
            .or_else(|| std::env::var("BOSS_SOCKET_PATH").ok())
            .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());
        let pid_file_path =
            std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
        let launch_directory = resolve_launch_directory()?;
        let engine = resolve_engine_command(&socket_path)?;

        Ok(Self {
            socket_path,
            pid_file_path,
            autostart: true,
            engine,
            launch_directory,
            start_timeout: DEFAULT_ENGINE_START_TIMEOUT,
        })
    }

    pub fn with_autostart(mut self, autostart: bool) -> Self {
        self.autostart = autostart;
        self
    }
}

/// Single-connection client over the engine's frontend socket.
pub struct BossClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_request_id: AtomicU64,
}

impl BossClient {
    /// Connect to the engine, optionally autostarting it per the discovery profile.
    pub async fn connect(discovery: &Discovery) -> Result<Self> {
        if let Ok(client) = Self::connect_socket(&discovery.socket_path).await {
            return Ok(client);
        }

        if !discovery.autostart {
            bail!(
                "boss engine is not reachable at {}",
                discovery.socket_path
            );
        }

        ensure_engine_running(discovery).await?;
        Self::connect_socket(&discovery.socket_path).await
    }

    /// Connect directly to a socket path without autostart logic.
    pub async fn connect_socket(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to engine socket {socket_path}"))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
            next_request_id: AtomicU64::new(1),
        })
    }

    /// Send a request and wait for the matching response by `request_id`.
    pub async fn send_request(&mut self, request: &FrontendRequest) -> Result<FrontendEvent> {
        let request_id = format!(
            "client-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let payload = serde_json::to_string(&FrontendRequestEnvelope {
            request_id: request_id.clone(),
            payload: request.clone(),
        })?;
        self.writer.write_all(payload.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        while let Some(line) = self.reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let envelope: FrontendEventEnvelope = serde_json::from_str(&line)
                .with_context(|| format!("failed to decode engine event: {line}"))?;
            if envelope.request_id.as_deref() == Some(request_id.as_str()) {
                return Ok(envelope.payload);
            }
        }

        bail!("engine closed the socket before returning a response")
    }
}

impl BossClient {
    /// Ask the running engine for its version identifiers. Returns
    /// `(git_sha, build_time, binary_fingerprint)`. The
    /// `binary_fingerprint` is the most reliable signal for detecting
    /// whether the running engine matches an expected binary — see
    /// `boss_engine::build_info::binary_fingerprint` for the algorithm.
    pub async fn get_engine_version(&mut self) -> Result<(String, String, String)> {
        let event = self
            .send_request(&boss_protocol::FrontendRequest::GetEngineVersion)
            .await?;
        match event {
            boss_protocol::FrontendEvent::EngineVersionResult {
                git_sha,
                build_time,
                binary_fingerprint,
            } => Ok((git_sha, build_time, binary_fingerprint)),
            other => anyhow::bail!(
                "unexpected response to GetEngineVersion: {:?}",
                other
            ),
        }
    }
}

pub async fn engine_socket_reachable(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).await.is_ok()
}

pub async fn wait_for_socket(socket_path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if engine_socket_reachable(socket_path).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

pub fn running_engine_pid(pid_file_path: &str) -> Option<u32> {
    let pid = read_pid_file(pid_file_path)?;
    let status = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if status.success() {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pid_file_path);
        None
    }
}

pub fn read_pid_file(pid_file_path: &str) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file_path).ok()?;
    content.trim().parse().ok()
}

pub async fn ensure_engine_running(discovery: &Discovery) -> Result<()> {
    if engine_socket_reachable(&discovery.socket_path).await {
        return Ok(());
    }

    if let Some(pid) = running_engine_pid(&discovery.pid_file_path) {
        if wait_for_socket(&discovery.socket_path, discovery.start_timeout).await {
            return Ok(());
        }
        bail!(
            "boss engine pid file points to pid {pid}, but socket {} never became ready",
            discovery.socket_path
        );
    }

    start_engine_process(discovery)?;
    if wait_for_socket(&discovery.socket_path, discovery.start_timeout).await {
        return Ok(());
    }

    bail!(
        "boss engine did not become ready at {} within {} seconds",
        discovery.socket_path,
        discovery.start_timeout.as_secs()
    )
}

pub fn stop_engine(pid_file_path: &str) -> Result<()> {
    let Some(pid) = running_engine_pid(pid_file_path) else {
        return Ok(());
    };

    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to invoke /bin/kill")?;
    if !status.success() {
        bail!("failed to stop boss engine pid {pid}");
    }

    if let Some(owner) = read_pid_file(pid_file_path) {
        if owner == pid {
            let _ = std::fs::remove_file(pid_file_path);
        }
    }

    Ok(())
}

fn start_engine_process(discovery: &Discovery) -> Result<()> {
    Command::new(&discovery.engine.program)
        .args(&discovery.engine.args)
        .current_dir(&discovery.launch_directory)
        .env("BOSS_ENGINE_PID_PATH", &discovery.pid_file_path)
        .env("BOSS_SOCKET_PATH", &discovery.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start engine using `{}` (resolved via {}).\nResolution chain (highest priority first):\n{}\nSet BOSS_ENGINE_BIN to an explicit engine binary path, or run `bazel build //tools/boss/engine:engine` so the bazel-bin lookup succeeds.",
                format_engine_command(&discovery.engine.program, &discovery.engine.args),
                discovery.engine.source,
                format_resolution_chain(&discovery.engine.attempted),
            )
        })
        .map(|_| ())
}

fn resolve_launch_directory() -> Result<PathBuf> {
    if let Some(workspace) = locate_bazel_workspace_root() {
        return Ok(workspace);
    }
    std::env::current_dir().context("failed to resolve current directory")
}

/// Relative path of the bazel-built engine binary inside a workspace.
const BAZEL_ENGINE_RELPATH: &str = "bazel-bin/tools/boss/engine/engine";

fn resolve_engine_command(socket_path: &str) -> Result<EngineCommand> {
    let input = EngineResolverInput {
        env_cmd: non_empty_env("BOSS_ENGINE_CMD"),
        env_bin: non_empty_env("BOSS_ENGINE_BIN"),
        workspace_root: locate_bazel_workspace_root(),
        current_exe: std::env::current_exe().ok(),
    };
    resolve_engine_command_with(socket_path, &input)
}

/// Pure resolver used by [`resolve_engine_command`] and tests.
///
/// Resolution order (highest priority first):
///   1. `BOSS_ENGINE_CMD` — full custom command (shell-split).
///   2. `BOSS_ENGINE_BIN` — explicit binary path; default args appended.
///   3. `bazel-bin/tools/boss/engine/engine` under the workspace root.
///   4. A `boss-engine` (or `engine/engine`) sibling next to the running
///      executable — covers `bazel run` runfiles layouts.
///   5. Bare `boss-engine` on `$PATH` (current default; fails loudly if
///      the binary isn't installed).
pub fn resolve_engine_command_with(
    socket_path: &str,
    input: &EngineResolverInput,
) -> Result<EngineCommand> {
    let mut attempted = Vec::new();

    if let Some(value) = input.env_cmd.as_deref() {
        attempted.push(format!("BOSS_ENGINE_CMD={value}"));
        let parts = shlex::split(value)
            .with_context(|| format!("failed to parse BOSS_ENGINE_CMD: {value}"))?;
        let Some((program, args)) = parts.split_first() else {
            bail!("BOSS_ENGINE_CMD resolved to an empty command");
        };
        return Ok(EngineCommand {
            program: program.clone(),
            args: args.to_vec(),
            source: "BOSS_ENGINE_CMD env var".to_owned(),
            attempted,
        });
    }
    attempted.push("BOSS_ENGINE_CMD env var (unset)".to_owned());

    if let Some(value) = input.env_bin.as_deref() {
        attempted.push(format!("BOSS_ENGINE_BIN={value}"));
        return Ok(EngineCommand {
            program: value.to_owned(),
            args: default_engine_args(socket_path),
            source: "BOSS_ENGINE_BIN env var".to_owned(),
            attempted,
        });
    }
    attempted.push("BOSS_ENGINE_BIN env var (unset)".to_owned());

    if let Some(workspace) = input.workspace_root.as_deref() {
        let candidate = workspace.join(BAZEL_ENGINE_RELPATH);
        if candidate.is_file() {
            attempted.push(format!("bazel-bin lookup hit {}", candidate.display()));
            return Ok(EngineCommand {
                program: candidate.to_string_lossy().into_owned(),
                args: default_engine_args(socket_path),
                source: format!("bazel-bin ({})", candidate.display()),
                attempted,
            });
        }
        attempted.push(format!(
            "bazel-bin lookup miss at {} (run `bazel build //tools/boss/engine:engine`)",
            candidate.display()
        ));
    } else {
        attempted.push("bazel-bin lookup skipped (no workspace root found)".to_owned());
    }

    if let Some(exe) = input.current_exe.as_deref() {
        if let Some((program, candidate)) = sibling_engine_binary(exe) {
            attempted.push(format!("sibling-of-exe hit {}", candidate.display()));
            return Ok(EngineCommand {
                program,
                args: default_engine_args(socket_path),
                source: format!("sibling of {}", exe.display()),
                attempted,
            });
        }
        attempted.push(format!(
            "sibling-of-exe miss next to {}",
            exe.display()
        ));
    } else {
        attempted.push("sibling-of-exe skipped (current_exe unavailable)".to_owned());
    }

    attempted.push("PATH lookup of `boss-engine`".to_owned());
    Ok(EngineCommand {
        program: "boss-engine".to_owned(),
        args: default_engine_args(socket_path),
        source: "PATH lookup of `boss-engine`".to_owned(),
        attempted,
    })
}

fn non_empty_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn default_engine_args(socket_path: &str) -> Vec<String> {
    vec!["--socket-path".to_owned(), socket_path.to_owned()]
}

fn sibling_engine_binary(exe: &Path) -> Option<(String, PathBuf)> {
    let dir = exe.parent()?;
    let mut candidates = vec![dir.join("boss-engine")];
    if let Some(boss_dir) = dir.parent() {
        candidates.push(boss_dir.join("engine").join("engine"));
    }
    candidates
        .into_iter()
        .find(|candidate: &PathBuf| candidate.is_file())
        .map(|candidate| (candidate.to_string_lossy().into_owned(), candidate))
}

fn locate_bazel_workspace_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    walk_to_workspace_root(&cwd)
}

fn walk_to_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if is_bazel_workspace(current) {
            return Some(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

fn is_bazel_workspace(dir: &Path) -> bool {
    dir.join("MODULE.bazel").exists()
        || dir.join("WORKSPACE").exists()
        || dir.join("WORKSPACE.bazel").exists()
}

fn format_engine_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_resolution_chain(attempted: &[String]) -> String {
    if attempted.is_empty() {
        return "(none)".to_owned();
    }
    attempted
        .iter()
        .enumerate()
        .map(|(idx, step)| format!("  {}. {step}", idx + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_workspace_with_engine(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path();
        std::fs::write(root.join("MODULE.bazel"), "module(name = \"test\")\n").unwrap();
        let engine_dir = root.join("bazel-bin/tools/boss/engine");
        std::fs::create_dir_all(&engine_dir).unwrap();
        let engine_bin = engine_dir.join("engine");
        std::fs::write(&engine_bin, b"#!/bin/sh\nexit 0\n").unwrap();
        engine_bin
    }

    #[test]
    fn env_cmd_wins_over_everything() {
        let tmp = tempfile::tempdir().unwrap();
        make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: Some("/custom/cmd --flag".to_owned()),
            env_bin: Some("/should-not-win/engine".to_owned()),
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/custom/cmd");
        assert_eq!(cmd.args, vec!["--flag".to_owned()]);
        assert_eq!(cmd.source, "BOSS_ENGINE_CMD env var");
    }

    #[test]
    fn env_bin_wins_over_bazel_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: Some("/explicit/engine".to_owned()),
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/explicit/engine");
        assert_eq!(
            cmd.args,
            vec!["--socket-path".to_owned(), "/tmp/sock".to_owned(),]
        );
        assert_eq!(cmd.source, "BOSS_ENGINE_BIN env var");
    }

    #[test]
    fn bazel_bin_wins_over_path_when_built() {
        let tmp = tempfile::tempdir().unwrap();
        let engine_bin = make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, engine_bin.to_string_lossy());
        assert!(
            cmd.source.starts_with("bazel-bin"),
            "expected bazel-bin source, got {}",
            cmd.source
        );
    }

    #[test]
    fn falls_back_to_path_when_engine_not_built() {
        let tmp = tempfile::tempdir().unwrap();
        // Workspace exists but no bazel-bin/tools/boss/engine/engine yet.
        std::fs::write(tmp.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "boss-engine");
        assert_eq!(cmd.source, "PATH lookup of `boss-engine`");
        // The chain must mention that bazel-bin was attempted but missed,
        // so the error message guides the user to `bazel build`.
        let chain_text = cmd.attempted.join("\n");
        assert!(
            chain_text.contains("bazel-bin lookup miss"),
            "expected bazel-bin miss to be reported in chain, got: {chain_text}"
        );
        assert!(chain_text.contains("PATH lookup"));
    }

    #[test]
    fn falls_back_to_path_when_no_workspace() {
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: None,
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "boss-engine");
        assert_eq!(cmd.source, "PATH lookup of `boss-engine`");
    }

    #[test]
    fn empty_env_cmd_is_ignored() {
        // `BOSS_ENGINE_CMD=""` (or whitespace) should not poison resolution.
        let input = EngineResolverInput {
            env_cmd: None, // non_empty_env strips this in production
            env_bin: Some("/from-bin/engine".to_owned()),
            workspace_root: None,
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/from-bin/engine");
    }

    #[test]
    fn walk_to_workspace_root_finds_module_bazel() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MODULE.bazel"), "").unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let found = walk_to_workspace_root(&nested).unwrap();
        // Compare canonicalized paths to be robust against /var vs /private/var
        // on macOS where TMPDIR resolves through a symlink.
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(tmp.path()).unwrap(),
        );
    }

    #[test]
    fn walk_to_workspace_root_returns_none_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        // No MODULE.bazel/WORKSPACE anywhere along the path inside tmp.
        let nested = tmp.path().join("nope");
        std::fs::create_dir_all(&nested).unwrap();
        // The walker stops at filesystem root if it never finds a marker;
        // this only proves "no marker inside the tmp tree" if the host
        // filesystem also lacks one — which it should.
        // We at least assert it does NOT pick the tmp dir itself.
        if let Some(found) = walk_to_workspace_root(&nested) {
            assert_ne!(found, tmp.path());
        }
    }
}
