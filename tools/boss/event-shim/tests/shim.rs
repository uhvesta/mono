//! End-to-end shim test: spawn the binary with stdin pointed at a
//! payload and `BOSS_EVENTS_SOCKET` pointed at a temp Unix socket we
//! listen on, then verify the bytes that arrive on the socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

fn shim_binary() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests of a
    // package that produces a binary.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_boss-event"))
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

    let mut child = Command::new(shim_binary())
        .env("BOSS_EVENTS_SOCKET", &socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child.stdin.as_mut().unwrap().write_all(payload).unwrap();
    drop(child.stdin.take());

    let status = child.wait().unwrap();
    assert!(status.success(), "shim exit status: {status}");

    let received = server
        .join()
        .expect("server thread panicked");
    assert_eq!(received, payload);
}

#[test]
fn fails_when_env_var_unset() {
    let mut child = Command::new(shim_binary())
        .env_remove("BOSS_EVENTS_SOCKET")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{}")
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("BOSS_EVENTS_SOCKET"),
        "stderr was: {stderr}"
    );
}

#[test]
fn fails_when_stdin_is_empty() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("events.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();

    let mut child = Command::new(shim_binary())
        .env("BOSS_EVENTS_SOCKET", &socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("empty"), "stderr was: {stderr}");
}

#[test]
fn fails_when_socket_does_not_exist() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("never-bound.sock");

    let mut child = Command::new(shim_binary())
        .env("BOSS_EVENTS_SOCKET", &socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"x\":1}")
        .unwrap();
    drop(child.stdin.take());

    let status = child
        .wait_timeout_workaround(Duration::from_secs(5))
        .expect("shim hung");
    assert!(!status.success());
}

trait WaitTimeoutWorkaround {
    fn wait_timeout_workaround(self, dur: Duration) -> Option<std::process::ExitStatus>;
}

impl WaitTimeoutWorkaround for std::process::Child {
    fn wait_timeout_workaround(mut self, dur: Duration) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + dur;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = self.kill();
                        return None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }
}
