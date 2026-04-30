//! Engine events socket — accepts connections from `boss-event` shims
//! running inside leased worker workspaces, looks up the connecting
//! peer's pid via `LOCAL_PEERPID`, decodes the JSON hook payload via
//! [`boss_protocol::normalize_hook_event`], and produces typed
//! [`IncomingHookEvent`]s annotated with the peer pid.
//!
//! Phase 6c ships the listener + PEERPID lookup + decode pipeline.
//! Lease correlation (peer pid → lease id) is wired in Phase 6f, when
//! the spawn flow gives the engine a registry of (worker pid → lease)
//! mappings to consult here.
//!
//! macOS-only. The `LOCAL_PEERPID` getsockopt is not portable; Boss
//! itself is macOS-only so this is consistent with the rest of the
//! engine.

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

/// One hook event after peer-pid lookup and normalization.
///
/// `peer_pid` is best-effort: macOS's `LOCAL_PEERPID` returns
/// `ENOTCONN` once the peer has closed its end, and the shim closes
/// immediately after writing. Callers that need a guaranteed pid must
/// look it up synchronously right after `accept()` (before any async
/// yield) and not rely on `peer_pid` alone for security decisions —
/// the lease registry from Phase 6f is the authoritative source.
#[derive(Debug, Clone)]
pub struct IncomingHookEvent {
    pub peer_pid: Option<libc::pid_t>,
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

/// Bind the events socket at `path`, removing any stale file there,
/// then chmod to 0600 so only the boss-engine user can connect.
pub fn bind_events_socket(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(listener)
}

/// Look up the peer pid of a connected stream socket via
/// `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`.
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

#[cfg(not(target_os = "macos"))]
pub fn peer_pid(_stream: &UnixStream) -> io::Result<libc::pid_t> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "LOCAL_PEERPID is only supported on macOS",
    ))
}

/// Read a connection to EOF and produce a typed IncomingHookEvent.
/// The shim half-closes its write side after writing the full hook
/// payload, so EOF is the message boundary.
///
/// Captures `LOCAL_PEERPID` synchronously before any await; if the
/// shim has already closed by then (its write is fast, then it
/// exits), the pid lookup may fail with `ENOTCONN` and the event is
/// returned with `peer_pid: None`.
pub async fn handle_connection(stream: UnixStream) -> Result<IncomingHookEvent, SocketError> {
    let peer_pid_value = peer_pid(&stream).ok();
    let mut stream = stream;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    let event = normalize_hook_event(&raw)?;
    Ok(IncomingHookEvent {
        peer_pid: peer_pid_value,
        event,
    })
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
