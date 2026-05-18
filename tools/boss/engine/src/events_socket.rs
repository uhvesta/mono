//! Engine events socket — accepts connections from `boss-event` shims
//! running inside leased worker workspaces, looks up the connecting
//! peer's pid via `LOCAL_PEERPID`, decodes the JSON hook payload via
//! [`boss_protocol::normalize_hook_event`], and produces typed
//! [`IncomingHookEvent`]s annotated with the peer pid and (when the
//! peer's process tree is registered with [`crate::worker_registry`])
//! the matching `run_id`.
//!
//! Cross-platform: macOS uses `LOCAL_PEERPID`, Linux uses `SO_PEERCRED`.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use boss_protocol::{NormalizeError, WorkerEvent, normalize_hook_event};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

/// `level` for `getsockopt(SOL_LOCAL, LOCAL_PEERPID)` on macOS.
#[cfg(target_os = "macos")]
const SOL_LOCAL: libc::c_int = 0;
/// `optname` for the LOCAL_PEERPID getsockopt on macOS.
#[cfg(target_os = "macos")]
const LOCAL_PEERPID: libc::c_int = 0x002;

/// One hook event after peer-pid lookup, payload extraction, and
/// normalization.
///
/// `peer_pid` is best-effort: the peer-pid lookup may return an error
/// once the peer has closed its end (e.g. `ENOTCONN` on macOS), and
/// the shim closes immediately after writing. Callers that need a guaranteed pid must
/// look it up synchronously right after `accept()` (before any async
/// yield) and not rely on `peer_pid` alone for security decisions —
/// the lease registry is the authoritative source.
///
/// `run_id` is extracted from the `_boss_run_id` field in the hook
/// payload, which the event-shim embeds whenever `BOSS_RUN_ID` is set
/// in its environment. The worker-spawn flow always sets this.
///
/// `transcript_path` is the verbatim `transcript_path` field claude
/// stamps on every hook payload — `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
/// We surface it here so the engine can persist it on `WorkRun` the
/// first time we see it; the live-status summarizer loop reads that
/// row to know which file to tail. Without this, `transcript_path`
/// stays NULL forever and the summarizer never gets past its
/// "no transcript path yet" early-out.
#[derive(Debug, Clone)]
pub struct IncomingHookEvent {
    pub peer_pid: Option<libc::pid_t>,
    pub run_id: Option<String>,
    pub transcript_path: Option<String>,
    pub event: WorkerEvent,
}

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("events socket io: {0}")]
    Io(#[from] io::Error),
    #[error("hook payload was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hook payload normalize: {0}")]
    Normalize(#[from] NormalizeError),
}

/// Bind+listen on the events socket at `path` and chmod the file to
/// 0600. This is synchronous — when this function returns Ok, the
/// socket is in the kernel's listening state, so a `connect()` from
/// another process will be queued in the accept backlog (not refused
/// with `ECONNREFUSED`) even before the caller polls `accept()` for
/// the first time. tokio's `UnixListener::bind` calls
/// `socket(2)` + `bind(2)` + `listen(2)` together; if `listen()`
/// fails the whole call returns the error, so there is no observable
/// "bound but not listening" intermediate state from the caller's
/// side.
///
/// Steps:
///   1. Ensure the parent directory exists.
///   2. Unconditionally try to unlink the path. A previous engine
///      that crashed without cleanup leaves a stale socket file
///      behind; if a fresh `bind()` ran without unlinking, on macOS
///      it would either return `EADDRINUSE` (if the kernel still
///      considers the inode bound) or — and this is the failure mode
///      the 2026-05-07 incident chased — the file would be replaced
///      but the new socket might never be put into the listen state
///      if some startup path reused the old fd. Just remove first.
///      `ENOENT` is the normal fresh-start case and is ignored;
///      every other error is fatal.
///   3. `UnixListener::bind` — atomic socket+bind+listen.
///   4. `chmod 0600` so only the boss-engine user can connect.
///
/// Errors are returned to the caller; the engine's `serve` propagates
/// them up to `main`, which records the failure in the audit log and
/// exits non-zero. A partially-bound socket never reaches the
/// dispatch loop.
pub fn bind_events_socket(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(
                events_socket_path = %path.display(),
                "events socket: unlinked stale file before bind",
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(path)?;
    tracing::info!(
        events_socket_path = %path.display(),
        "events socket: bind+listen succeeded",
    );
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(listener)
}

/// Look up the peer pid of a connected stream socket via
/// `getsockopt(SOL_LOCAL, LOCAL_PEERPID)` on macOS.
#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `fd` is borrowed from the caller's UnixStream and remains
    // valid for this call; `pid` and `len` are stack-local mutables and
    // their addresses are passed only to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Look up the peer pid of a connected stream socket via
/// `getsockopt(SO_PEERCRED)` on Linux.
#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is borrowed from the caller's UnixStream and remains
    // valid for this call; `cred` and `len` are stack-local mutables and
    // their addresses are passed only to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn peer_pid(_stream: &UnixStream) -> io::Result<libc::pid_t> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer pid lookup is not supported on this platform",
    ))
}

/// Read a connection to EOF and produce a typed IncomingHookEvent.
/// The shim half-closes its write side after writing the full hook
/// payload, so EOF is the message boundary.
///
/// Captures the peer pid synchronously before any await; if the
/// shim has already closed by then (its write is fast, then it
/// exits), the pid lookup may fail and the event is returned with
/// `peer_pid: None`.
///
/// `run_id` is extracted from the `_boss_run_id` field embedded in
/// the payload by the `boss-event` shim (sourced from `BOSS_RUN_ID` in
/// the worker's env). Every production event connection should carry
/// this field. If missing, a warning is logged but the event is
/// returned with `run_id: None`.
pub async fn handle_connection(
    stream: UnixStream,
) -> Result<IncomingHookEvent, SocketError> {
    let peer_pid_value = peer_pid(&stream).ok();
    let mut stream = stream;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    let payload_run_id = extract_run_id_from_payload(&raw);
    let run_id = if payload_run_id.is_none() {
        tracing::warn!("incoming hook event missing _boss_run_id field");
        None
    } else {
        payload_run_id
    };
    let transcript_path = extract_transcript_path_from_payload(&raw);
    let event = normalize_hook_event(&raw)?;
    Ok(IncomingHookEvent {
        peer_pid: peer_pid_value,
        run_id,
        transcript_path,
        event,
    })
}

/// Pull `_boss_run_id` out of the raw hook payload if the shim
/// embedded it. Empty strings are treated as missing so a stray
/// `BOSS_RUN_ID=` doesn't poison correlation with an empty id.
fn extract_run_id_from_payload(raw: &serde_json::Value) -> Option<String> {
    let s = raw.get("_boss_run_id")?.as_str()?;
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

/// Pull `transcript_path` out of the raw hook payload. Claude stamps
/// the absolute path to the session's JSONL transcript on every hook
/// payload it emits; the boss-event shim forwards the payload
/// unchanged, so we read the field straight off the wire. Empty
/// strings are treated as missing so we never persist a path that
/// `tokio::fs::File::open` would reject anyway.
fn extract_transcript_path_from_payload(raw: &serde_json::Value) -> Option<String> {
    let s = raw.get("transcript_path")?.as_str()?;
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use tempfile::TempDir;

    #[tokio::test]
    async fn bind_creates_socket_with_mode_0600() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();

        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[tokio::test]
    async fn bind_replaces_stale_socket_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        std::fs::write(&path, b"stale").unwrap();
        let _listener = bind_events_socket(&path).unwrap();
        assert!(path.exists());
        // The file is now a socket, not a regular file with "stale" content.
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
    }

    #[tokio::test]
    async fn bind_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");
        let path = nested.join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();
        assert!(path.exists());
    }

    /// Regression test for the 2026-05-07 incident: after
    /// `bind_events_socket` returns, the kernel must already be in the
    /// listen state. A `connect()` from a separate thread must
    /// succeed (not return ECONNREFUSED) even before the caller polls
    /// `accept()`. tokio's `UnixListener::bind` covers this — this
    /// test pins the contract so a refactor that splits bind from
    /// listen across async hops fails loudly.
    #[tokio::test]
    async fn connect_succeeds_immediately_after_bind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();
        // No `accept()` yet — the connect must be queued in the
        // backlog by the kernel, not refused.
        let path_for_thread = path.clone();
        let connected = std::thread::spawn(move || StdUnixStream::connect(&path_for_thread))
            .join()
            .unwrap();
        assert!(
            connected.is_ok(),
            "connect() right after bind must succeed, got {:?}",
            connected.err()
        );
    }

    /// A previous engine that crashed without cleanup leaves a
    /// dangling socket file. The new engine must unlink it cleanly
    /// and the rebound socket must be in the listen state. (The
    /// `bind_replaces_stale_socket_file` test above checks the file
    /// type swap; this one checks the listen-state of the rebound
    /// socket — the bug's load-bearing assertion.)
    #[tokio::test]
    async fn rebind_after_stale_file_listens() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");

        // Round 1: bind, then drop the listener. The on-disk socket
        // file persists (close(2) doesn't unlink AF_UNIX paths).
        {
            let _listener = bind_events_socket(&path).unwrap();
        }
        assert!(path.exists(), "stale socket file should remain after drop");

        // Round 2: rebind. Must unlink + listen successfully.
        let _listener = bind_events_socket(&path).unwrap();
        let path_for_thread = path.clone();
        let connected = std::thread::spawn(move || StdUnixStream::connect(&path_for_thread))
            .join()
            .unwrap();
        assert!(
            connected.is_ok(),
            "connect() after rebind must succeed, got {:?}",
            connected.err()
        );
    }

    #[tokio::test]
    async fn round_trip_hook_payload_through_socket() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // Mimic the shim: connect, write, then close. The peer_pid
        // lookup is best-effort under this race — it might be Some or
        // None depending on scheduling. We assert only on the event
        // payload here; the explicit pid-matching test below holds the
        // client alive for the duration of the lookup.
        let payload =
            br#"{"hook_event_name":"Stop","session_id":"sess-1","stop_hook_active":false}"#;
        let path_owned = path.clone();
        let client_task = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        client_task.await.unwrap();

        match incoming.event {
            WorkerEvent::Stop { session_id, .. } => assert_eq!(session_id, "sess-1"),
            other => panic!("expected Stop, got {other:?}"),
        }
        // Empty registry: no run_id correlation.
        assert_eq!(incoming.run_id, None);
    }

    #[tokio::test]
    async fn transcript_path_extracted_from_payload() {
        // Claude stamps `transcript_path` on every hook payload. We
        // surface it here so the engine can persist it on the
        // `work_runs` row — without this round-trip, the live-status
        // summarizer's tail watcher has no file to open and the per-slot
        // loop early-outs every tick on "no transcript path yet".
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(
                br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"transcript_path":"/home/u/.claude/projects/foo/sess-1.jsonl"}"#,
            ).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        client.await.unwrap();

        assert_eq!(
            incoming.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        );
    }

    #[tokio::test]
    async fn missing_transcript_path_is_none() {
        // Pre-live-status hook payloads (and the test fixtures still
        // around) won't carry the field. The extractor must surface
        // `None` rather than erroring or stalling the dispatcher.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream
                .write_all(br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false}"#)
                .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        client.await.unwrap();

        assert!(incoming.transcript_path.is_none());
    }

    #[tokio::test]
    async fn empty_transcript_path_is_none() {
        // An empty string would round-trip through SQLite into a
        // path the tail watcher would try (and fail) to open every
        // tick. Treat empty as missing, matching the `_boss_run_id`
        // policy.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(
                br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"transcript_path":""}"#,
            ).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        client.await.unwrap();

        assert!(incoming.transcript_path.is_none());
    }

    #[tokio::test]
    async fn run_id_extracted_from_payload_field() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // Empty registry — pid lookup will return None. The
        // `_boss_run_id` field embedded by the shim is the only path
        // by which the engine should resolve a run id today.
        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(
                br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"run-from-payload"}"#,
            ).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        client.await.unwrap();

        assert_eq!(incoming.run_id.as_deref(), Some("run-from-payload"));
    }

    #[tokio::test]
    async fn payload_run_id_wins_over_missing_fallback() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // The `_boss_run_id` field in the payload is the only path for
        // run correlation. This test confirms it's correctly extracted.
        let path_owned = path.clone();
        let (close_tx, close_rx) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(
                br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"run-from-payload"}"#,
            ).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let _ = close_rx.recv();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream).await.unwrap();
        assert_eq!(incoming.run_id.as_deref(), Some("run-from-payload"));

        close_tx.send(()).ok();
        client.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_json_yields_socket_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(b"not json").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream).await;
        client.await.unwrap();

        match result {
            Err(SocketError::Json(_)) => {}
            other => panic!("expected Json error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn known_event_with_unknown_kind_yields_normalize_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let payload = br#"{"session_id":"x","hook_event_name":"WeirdHook"}"#;
        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream).await;
        client.await.unwrap();

        match result {
            Err(SocketError::Normalize(NormalizeError::UnknownEvent(name))) => {
                assert_eq!(name, "WeirdHook");
            }
            other => panic!("expected Normalize/UnknownEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peer_pid_matches_self_when_client_stays_connected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // Hold the client open until the server has captured peer_pid.
        let (close_tx, close_rx) = std::sync::mpsc::channel::<()>();
        let path_owned = path.clone();
        let payload = b"{}";
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            // Block in the thread, keeping the stream alive (its
            // descriptor is still owned by `stream`) until the server
            // signals we can drop it.
            let _ = close_rx.recv();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let observed_pid = peer_pid(&stream).unwrap();
        let self_pid = std::process::id() as libc::pid_t;
        assert_eq!(observed_pid, self_pid);

        // Release the client.
        close_tx.send(()).ok();
        client.join().unwrap();
    }
}
