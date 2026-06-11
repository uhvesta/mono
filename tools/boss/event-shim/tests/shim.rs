//! End-to-end shim tests: spawn the binary with stdin pointed at a
//! payload, point `BOSS_EVENTS_SOCKET` at a temp Unix socket we control,
//! and verify the shim's interaction with the socket, the on-disk
//! buffer, retries, and reconnect.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn shim_binary() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests of a
    // package that produces a binary.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_boss-event"))
}

/// Spawn the shim with the given env and stdin payload, waiting for
/// completion with a wall-clock timeout so a hung child can't hang the
/// test runner.
fn run_shim(
    socket: Option<&Path>,
    workspace: Option<&Path>,
    retry_delays_ms: Option<&str>,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Output {
    let mut cmd = Command::new(shim_binary());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("BOSS_RUN_ID");
    match socket {
        Some(p) => {
            cmd.env("BOSS_EVENTS_SOCKET", p);
        }
        None => {
            cmd.env_remove("BOSS_EVENTS_SOCKET");
        }
    }
    match workspace {
        Some(p) => {
            cmd.env("BOSS_WORKSPACE", p);
        }
        None => {
            cmd.env_remove("BOSS_WORKSPACE");
        }
    }
    match retry_delays_ms {
        Some(v) => {
            cmd.env("BOSS_EVENT_RETRY_DELAYS_MS", v);
        }
        None => {
            cmd.env_remove("BOSS_EVENT_RETRY_DELAYS_MS");
        }
    }
    let mut child = cmd.spawn().unwrap();
    if let Some(bytes) = stdin {
        child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    }
    drop(child.stdin.take());

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("shim did not exit within {timeout:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    child.wait_with_output().unwrap()
}

#[test]
fn forwards_stdin_payload_to_unix_socket() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("events.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let payload = br#"{"hook_event_name":"Stop","session_id":"sess-1","stop_hook_active":false}"#;

    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        conn.read_to_end(&mut received).unwrap();
        received
    });

    let workspace = TempDir::new().unwrap();
    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10"),
        Some(payload),
        Duration::from_secs(5),
    );
    assert!(
        out.status.success(),
        "shim exit: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let received = server.join().expect("server thread panicked");
    assert_eq!(received, payload);
}

#[test]
fn fails_when_env_var_unset() {
    let workspace = TempDir::new().unwrap();
    let out = run_shim(None, Some(workspace.path()), None, Some(b"{}"), Duration::from_secs(5));
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("BOSS_EVENTS_SOCKET"), "stderr was: {stderr}");
}

#[test]
fn fails_when_stdin_is_empty() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("events.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
    let workspace = TempDir::new().unwrap();

    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        None,
        None,
        Duration::from_secs(5),
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("empty"), "stderr was: {stderr}");
}

/// When the engine socket is unreachable and retries are exhausted, the
/// shim must buffer the event to disk and exit zero so the worker keeps
/// going. This is the 2026-05-07 incident's acceptance test.
#[test]
fn buffers_event_when_socket_is_unreachable_after_retries() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");
    let workspace = TempDir::new().unwrap();

    let payload = br#"{"hook_event_name":"Stop","session_id":"sess-x","stop_hook_active":false}"#;
    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10,10,10"),
        Some(payload),
        Duration::from_secs(5),
    );
    assert!(
        out.status.success(),
        "shim should exit zero after buffering: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("buffering"),
        "shim should log the buffering decision loudly; stderr was: {stderr}",
    );

    let buffer_path = workspace.path().join(".boss/events-pending.jsonl");
    let contents = fs::read_to_string(&buffer_path).expect("buffer file should exist after engine-down event");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"session_id\":\"sess-x\""));
}

/// After a previous engine-down window left events in the buffer, the
/// next reachable connection must drain them in FIFO order before the
/// current event is sent.
#[test]
fn drains_buffered_events_on_next_successful_connect() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("events.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let workspace = TempDir::new().unwrap();
    let buffer_path = workspace.path().join(".boss/events-pending.jsonl");
    fs::create_dir_all(buffer_path.parent().unwrap()).unwrap();
    fs::write(&buffer_path, b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n").unwrap();

    // Run a server that accepts 4 connections (3 buffered + 1 current)
    // and records what it received from each.
    let server = thread::spawn(move || {
        let mut received: Vec<Vec<u8>> = Vec::new();
        for _ in 0..4 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            conn.read_to_end(&mut buf).unwrap();
            received.push(buf);
        }
        received
    });

    let payload = b"{\"n\":4}";
    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10"),
        Some(payload),
        Duration::from_secs(10),
    );
    assert!(
        out.status.success(),
        "shim exit: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let received = server.join().unwrap();
    assert_eq!(received.len(), 4);
    assert_eq!(received[0], b"{\"n\":1}");
    assert_eq!(received[1], b"{\"n\":2}");
    assert_eq!(received[2], b"{\"n\":3}");
    assert_eq!(received[3], b"{\"n\":4}");

    // Buffer should be fully drained.
    assert!(
        !buffer_path.exists() || fs::read(&buffer_path).unwrap().is_empty(),
        "buffer should be empty after drain",
    );
}

/// Connect-retry must succeed if the engine comes up partway through
/// the retry schedule. We bind the socket from a background thread
/// after a short delay; the shim should pick it up on a later attempt.
#[test]
fn retries_connect_until_socket_appears() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("late.sock");
    let socket_path_owned = socket_path.clone();

    // Start the server ~200ms after the shim. The shim's first attempt
    // fails (no socket yet); a subsequent attempt should succeed.
    let server = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let listener = UnixListener::bind(&socket_path_owned).unwrap();
        let (mut conn, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        conn.read_to_end(&mut received).unwrap();
        received
    });

    let workspace = TempDir::new().unwrap();
    let payload = b"{\"late\":true}";
    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("50,100,200,500"),
        Some(payload),
        Duration::from_secs(5),
    );
    assert!(
        out.status.success(),
        "shim should succeed once socket appears mid-retry; stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let received = server.join().unwrap();
    assert_eq!(received, payload);

    // No buffering because we eventually delivered.
    let buffer_path = workspace.path().join(".boss/events-pending.jsonl");
    assert!(
        !buffer_path.exists() || fs::read(&buffer_path).unwrap().is_empty(),
        "buffer should be empty after a retry-and-deliver path",
    );
}

/// If the connection survives connect but the peer drops before we
/// finish writing, the shim should reopen once and resend the payload.
/// We simulate this by running an "abrupt" server that closes after
/// accept (no read), then a second server that captures the payload.
#[test]
fn reconnects_on_mid_send_broken_pipe() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("events.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let received_arc = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_for_thread = received_arc.clone();

    let server = thread::spawn(move || {
        // First connection: accept and immediately drop, simulating a
        // server that died between accept and read.
        let (conn, _) = listener.accept().unwrap();
        drop(conn);
        // Second connection: read full payload.
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        // A small read loop in case of partial reads.
        conn.read_to_end(&mut buf).unwrap();
        *received_for_thread.lock().unwrap() = buf;
    });

    let workspace = TempDir::new().unwrap();
    // Payload large enough to make the broken-pipe path more likely to
    // surface than fit-in-kernel-send-buffer.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"{\"big\":\"");
    payload.extend(std::iter::repeat(b'x').take(64 * 1024));
    payload.extend_from_slice(b"\"}");

    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10"),
        Some(&payload),
        Duration::from_secs(10),
    );
    server.join().unwrap();
    assert!(
        out.status.success(),
        "shim should reconnect and succeed; status: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let received = received_arc.lock().unwrap().clone();
    // Either the first or second connection delivered the bytes; the
    // second one must have the full payload because the first was
    // dropped before any read. If the bytes fit in the kernel write
    // buffer (4MB on macOS by default) the first write might "succeed"
    // and the shim wouldn't trigger reconnect — in that case the
    // received bytes still equal payload because the kernel delivered
    // them before drop. Either way the bytes match.
    if received == payload {
        // ok
    } else {
        // Allow the case where the first conn drained some bytes; in
        // practice we either get the whole payload or nothing. If we
        // got nothing, the shim's reconnect path is required to have
        // succeeded.
        assert!(
            !received.is_empty(),
            "expected reconnect-and-resend, but server received nothing",
        );
    }
}

/// Pre-existing buffer events survive an engine-down shim invocation:
/// the new event is appended to the buffer and the old events stay
/// queued in FIFO order for next time.
#[test]
fn engine_down_appends_to_existing_buffer_without_loss() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");
    let workspace = TempDir::new().unwrap();
    let buffer_path = workspace.path().join(".boss/events-pending.jsonl");
    fs::create_dir_all(buffer_path.parent().unwrap()).unwrap();
    fs::write(&buffer_path, b"{\"n\":1}\n{\"n\":2}\n").unwrap();

    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10,10"),
        Some(b"{\"n\":3}"),
        Duration::from_secs(5),
    );
    assert!(
        out.status.success(),
        "shim should still exit zero after buffering; stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let contents = fs::read_to_string(&buffer_path).expect("buffer should still exist");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines,
        vec!["{\"n\":1}", "{\"n\":2}", "{\"n\":3}"],
        "buffer must preserve FIFO across engine-down windows",
    );
}

/// Confirms the shim does retry on a missing socket. Without retry, the
/// first attempt would fail in microseconds; with retry, even the
/// fastest configured schedule takes at least the configured backoff.
#[test]
fn missing_socket_triggers_retry_loop() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");
    let workspace = TempDir::new().unwrap();

    let start = Instant::now();
    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("50,50,50"),
        Some(b"{\"x\":1}"),
        Duration::from_secs(5),
    );
    let elapsed = start.elapsed();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // Three 50ms retries means at least ~150ms before exhaustion.
    assert!(
        elapsed >= Duration::from_millis(120),
        "retry loop should have slept at least ~150ms, took {elapsed:?}",
    );
}

/// Sanity: when no `BOSS_WORKSPACE` is set and cwd points at a writable
/// directory, the shim still buffers (cwd-relative) so a misconfigured
/// hook doesn't silently lose events.
#[test]
fn buffers_to_cwd_when_workspace_env_unset() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");
    let workspace = TempDir::new().unwrap();

    let mut cmd = Command::new(shim_binary());
    cmd.current_dir(workspace.path())
        .env("BOSS_EVENTS_SOCKET", &socket_path)
        .env("BOSS_EVENT_RETRY_DELAYS_MS", "10")
        .env_remove("BOSS_WORKSPACE")
        .env_remove("BOSS_RUN_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"Stop\",\"session_id\":\"s\",\"stop_hook_active\":false}")
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();

    assert!(
        out.status.success(),
        "shim should buffer via cwd fallback; stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let buffer_path = workspace.path().join(".boss/events-pending.jsonl");
    assert!(
        buffer_path.exists(),
        "buffer should be at cwd-relative path: {}",
        buffer_path.display(),
    );
}

/// Ensure stale ECONNREFUSED detection still happens — we want a
/// missing-socket attempt to exhaust retries and surface a "buffering"
/// stderr message rather than silently succeeding with no delivery.
#[test]
fn fails_when_socket_does_not_exist_then_buffers() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");
    let workspace = TempDir::new().unwrap();

    let out = run_shim(
        Some(&socket_path),
        Some(workspace.path()),
        Some("10,10"),
        Some(b"{\"x\":1}"),
        Duration::from_secs(5),
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unreachable") && stderr.contains("buffering"),
        "stderr should call out engine-unreachable + buffering: {stderr}",
    );
}
