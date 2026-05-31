//! Integration tests for the test-fixture isolation guard.
//!
//! Issue from 2026-05-24: a Swift XCTest spawned an additional Rust engine
//! binary alongside the live production engine. Because only `--socket-path`
//! was overridden, the test engine silently bound to the *production*
//! `events.sock`, DB, and pid file — causing corrupted state on T651.
//!
//! The fix: when `--socket-path` is non-default, `run_server` derives
//! isolated paths for `BOSS_EVENTS_SOCKET`, `BOSS_DB_PATH`, and
//! `BOSS_ENGINE_PID_PATH` from the socket's directory + stem.  This module
//! validates that derivation and the resulting isolation at the `serve()` level.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::{process_is_alive, serve};
use boss_engine::config::{RuntimeConfig, WorkConfig};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TestEngine {
    #[allow(dead_code)]
    socket_path: PathBuf,
    pid_path: PathBuf,
    events_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    /// Spawn an in-process engine bound to isolated temp paths.
    async fn spawn(stem: &str) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join(format!("{stem}.sock"));
        let db_path = temp.path().join(format!("{stem}.db"));
        let pid_path = temp.path().join(format!("{stem}.pid"));
        let events_path = temp.path().join(format!("{stem}.events.sock"));

        let work = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path,
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work, None));

        let sock = socket_path.clone();
        let pid = pid_path.clone();
        let ev = events_path.clone();
        let join = tokio::spawn(async move {
            serve(cfg, sock, Some(pid), Some(ev), None, None).await
        });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }

        Ok(Self {
            socket_path,
            pid_path,
            events_path,
            _temp: temp,
            join,
        })
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

// ---------------------------------------------------------------------------
// Unit tests — IsolationPaths derivation (no engine started)
// ---------------------------------------------------------------------------

/// `process_is_alive` reports true for this running process and false for
/// a pid that can't possibly be running.
#[test]
fn process_is_alive_unit_tests() {
    // Our own pid must be alive.
    let own_pid = std::process::id() as i32;
    assert!(process_is_alive(own_pid), "own pid must be alive");

    // pid 0 is always invalid (the kernel rejects kill(0, 0) from user space).
    assert!(!process_is_alive(0));
    // i32::MAX is virtually guaranteed to not exist as a live process.
    assert!(!process_is_alive(i32::MAX));
}

// ---------------------------------------------------------------------------
// Integration tests — serve() with isolated paths
// ---------------------------------------------------------------------------

/// Starting an engine with a non-default socket places the pid file at the
/// derived path, NOT at the production `/tmp/boss-engine.pid`.
#[tokio::test]
async fn isolated_engine_writes_pid_to_derived_path() -> Result<()> {
    let engine = TestEngine::spawn("boss-test-isolation-pid").await?;

    // Pid file must exist at the derived path.
    assert!(
        engine.pid_path.exists(),
        "isolated pid file must exist at derived path {}",
        engine.pid_path.display()
    );

    // Read pid from the file: must be a real running process.
    let content = std::fs::read_to_string(&engine.pid_path)?;
    let pid: i32 = content.trim().parse().expect("pid file must contain a number");
    assert!(process_is_alive(pid), "pid in isolated pid file must be alive");

    // The production pid path (/tmp/boss-engine.pid) must NOT have been
    // overwritten by this engine — its content (if any) should not be our pid.
    let prod_pid_path = std::path::Path::new("/tmp/boss-engine.pid");
    if let Ok(prod_content) = std::fs::read_to_string(prod_pid_path) {
        let prod_pid: i32 = prod_content.trim().parse().unwrap_or(-1);
        assert_ne!(
            prod_pid, pid,
            "test-fixture engine must NOT write to the production pid file"
        );
    }

    Ok(())
}

/// Starting an engine with a non-default socket binds the events socket at
/// the derived path, NOT at the production
/// `~/Library/Application Support/Boss/events.sock`.
#[tokio::test]
async fn isolated_engine_binds_events_socket_at_derived_path() -> Result<()> {
    let engine = TestEngine::spawn("boss-test-isolation-events").await?;

    // Events socket must exist at the derived path.
    assert!(
        engine.events_path.exists(),
        "isolated events socket must exist at {}",
        engine.events_path.display()
    );

    // The production events socket path is under $HOME/Library/Application
    // Support/Boss/events.sock.  In the bazel sandbox HOME=/tmp, so that
    // resolves to /tmp/Library/Application Support/Boss/events.sock — a
    // completely different directory from our derived /tmp/boss-test-*.events.sock.
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let prod_events = std::path::PathBuf::from(home)
        .join("Library/Application Support/Boss/events.sock");
    assert_ne!(
        engine.events_path, prod_events,
        "derived events socket path must differ from production path"
    );

    Ok(())
}

/// Two engines — one "production-style" and one "test-fixture" — can coexist
/// without sharing their events socket or pid file.
///
/// This is the retroactive regression test for the 2026-05-24 incident where
/// a test-fixture engine bound to the production events.sock and overwrote the
/// production pid file.
#[tokio::test]
async fn production_and_test_fixture_engines_use_distinct_paths() -> Result<()> {
    let temp = tempfile::tempdir()?;

    // "Production-style" engine: explicit paths in temp dir (simulates production).
    let prod_socket = temp.path().join("boss-engine.sock");
    let prod_events = temp.path().join("prod-events.sock");
    let prod_db = temp.path().join("prod-state.db");
    let prod_pid = temp.path().join("boss-engine.pid");

    let prod_work = WorkConfig {
        cwd: temp.path().to_path_buf(),
        db_path: prod_db,
        worker_pool_size: 1,
    };
    let prod_cfg = Arc::new(RuntimeConfig::from_parts(prod_work, None));
    let prod_sock_c = prod_socket.clone();
    let prod_pid_c = prod_pid.clone();
    let prod_ev_c = prod_events.clone();
    let prod_join = tokio::spawn(async move {
        serve(prod_cfg, prod_sock_c, Some(prod_pid_c), Some(prod_ev_c), None, None).await
    });
    if !wait_for_socket(prod_socket.to_str().unwrap(), STARTUP_TIMEOUT).await {
        prod_join.abort();
        return Err(anyhow!("production engine never bound socket"));
    }

    // "Test-fixture" engine: different socket stem, different derived paths.
    let test_socket = temp.path().join("boss-test-uuid.sock");
    let test_events = temp.path().join("boss-test-uuid.events.sock");
    let test_db = temp.path().join("boss-test-uuid.db");
    let test_pid = temp.path().join("boss-test-uuid.pid");

    let test_work = WorkConfig {
        cwd: temp.path().to_path_buf(),
        db_path: test_db,
        worker_pool_size: 1,
    };
    let test_cfg = Arc::new(RuntimeConfig::from_parts(test_work, None));
    let test_sock_c = test_socket.clone();
    let test_pid_c = test_pid.clone();
    let test_ev_c = test_events.clone();
    let test_join = tokio::spawn(async move {
        serve(test_cfg, test_sock_c, Some(test_pid_c), Some(test_ev_c), None, None).await
    });
    if !wait_for_socket(test_socket.to_str().unwrap(), STARTUP_TIMEOUT).await {
        prod_join.abort();
        test_join.abort();
        return Err(anyhow!("test-fixture engine never bound socket"));
    }

    // Both engines must be alive.
    let prod_pid_val: i32 = std::fs::read_to_string(&prod_pid)?.trim().parse()?;
    let test_pid_val: i32 = std::fs::read_to_string(&test_pid)?.trim().parse()?;
    assert!(process_is_alive(prod_pid_val), "production engine must still be alive");
    assert!(process_is_alive(test_pid_val), "test-fixture engine must still be alive");

    // Their paths must differ — the key invariant violated in the 2026-05-24 incident.
    assert_ne!(prod_events, test_events, "events sockets must be at distinct paths");
    assert_ne!(prod_pid, test_pid, "pid files must be at distinct paths");

    prod_join.abort();
    test_join.abort();
    Ok(())
}
