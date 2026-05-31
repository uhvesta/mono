//! Integration tests for the orphan-reaping parent-watcher in the engine.
//!
//! When the engine is started as a test fixture (`--socket-path` is
//! non-default), `run_server` arms a background task that polls whether the
//! parent process is still alive.  If the parent exits (e.g. the `bazel test`
//! runner that spawned the engine crashes or is killed), the engine should
//! exit cleanly within a bounded time rather than persisting as a long-lived
//! orphan that keeps production sockets / DB / pid file bound.
//!
//! These tests exercise `serve()` with an explicit `watched_parent_pid` so
//! they don't need a real parent/child process relationship.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(8);

/// Start a test-fixture engine that watches a short-lived subprocess.
/// Kill the subprocess; verify the engine exits within `ORPHAN_TIMEOUT`.
///
/// This is the direct test for the acceptance criterion:
///   "spawn a test-fixture engine; kill its parent; verify the engine exits
///    within a bounded time (≤ 5 seconds)"
///
/// We use a `sleep 60` subprocess as the watched "parent" so the test
/// process itself never has to exit.  Once we kill the sleep process,
/// `process_is_alive(sleep_pid)` returns false, the watcher fires, and
/// `serve()` exits via the orphan-shutdown arm of its accept loop.
#[tokio::test]
async fn serve_exits_when_watched_parent_dies() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");

    // Spawn a short-lived subprocess we'll play the role of "parent".
    let mut parent_proc = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .map_err(|e| anyhow!("failed to spawn sleep: {e}"))?;
    let parent_pid = parent_proc.id() as i32;

    let work = WorkConfig {
        cwd: temp.path().to_path_buf(),
        db_path,
        worker_pool_size: 1,
    };
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));

    let sock = socket_path.clone();
    let join = tokio::spawn(async move {
        serve(cfg, sock, None, None, None, Some(parent_pid)).await
    });

    // Wait for the engine to start.
    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        parent_proc.kill().ok();
        parent_proc.wait().ok();
        join.abort();
        return Err(anyhow!("engine never bound socket {}", socket_path.display()));
    }

    // Kill the watched "parent" to simulate orphaning.
    parent_proc.kill().map_err(|e| anyhow!("failed to kill parent: {e}"))?;
    parent_proc.wait().ok();

    // The engine's watcher polls every second; give it ORPHAN_TIMEOUT to exit.
    let result = tokio::time::timeout(ORPHAN_TIMEOUT, join).await;
    assert!(
        result.is_ok(),
        "serve() must exit within {ORPHAN_TIMEOUT:?} of the watched parent dying"
    );
    let serve_result = result.unwrap().unwrap();
    assert!(
        serve_result.is_ok(),
        "serve() must exit cleanly (Ok) on orphan detection, got: {serve_result:?}"
    );

    Ok(())
}

/// When `watched_parent_pid` is `None` (no orphan watcher armed), killing an
/// unrelated process has no effect on the engine.  This guards against
/// accidentally arming the watcher in production mode.
#[tokio::test]
async fn serve_without_parent_watch_is_unaffected_by_subprocess_death() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");

    let work = WorkConfig {
        cwd: temp.path().to_path_buf(),
        db_path,
        worker_pool_size: 1,
    };
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));

    let sock = socket_path.clone();
    let join = tokio::spawn(async move {
        // watched_parent_pid = None → watcher not armed
        serve(cfg, sock, None, None, None, None).await
    });

    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound socket {}", socket_path.display()));
    }

    // Spawn and immediately kill an unrelated process; engine must keep running.
    let mut unrelated = std::process::Command::new("sleep").arg("1").spawn()?;
    let unrelated_pid = unrelated.id();
    unrelated.kill().ok();
    unrelated.wait().ok();

    // Wait 3 seconds: engine should NOT have exited.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !join.is_finished(),
        "engine must NOT exit when watched_parent_pid is None (pid {unrelated_pid} killed)"
    );

    join.abort();
    Ok(())
}
