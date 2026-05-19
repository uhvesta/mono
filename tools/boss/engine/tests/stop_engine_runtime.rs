//! Regression test for #720: `boss_client::stop_engine` must not panic
//! when called from within a tokio runtime.
//!
//! Pre-fix shape: `stop_engine` was a synchronous function whose
//! happy path built a `new_current_thread` tokio runtime and called
//! `block_on` on it. Every real caller in the CLI lives under
//! `#[tokio::main]`, so `block_on` panicked with "Cannot start a
//! runtime from within a runtime", the documented SIGTERM fallback
//! never ran, and `boss engine stop` exited 101 leaving the engine
//! alive.
//!
//! Post-fix shape: `stop_engine` is `async`, awaits the shutdown
//! RPC on the caller's runtime, and reaches the SIGTERM fallback if
//! the RPC fails.
//!
//! Coverage strategy: boot a real engine in-process (so the
//! shutdown RPC has a real socket + token to talk to), write a
//! PID file pointing at this process so `running_engine_pid`
//! returns Some, and call `stop_engine(...).await` from within a
//! `#[tokio::test]`. With the bug present this panics; with the
//! fix it returns `Ok(())` after the engine drains.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    token_path: PathBuf,
    pid_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let token_path = temp.path().join("engine-control.token");
        let pid_path = temp.path().join("engine.pid");

        // Write a PID file that names this test process. `stop_engine`
        // uses `kill -0` to verify the PID is alive before it bothers
        // with the RPC, so we need a process that actually exists.
        std::fs::write(&pid_path, std::process::id().to_string())?;

        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path,
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let token_for_serve = token_path.clone();
        let join = tokio::spawn(async move {
            serve(cfg, socket_for_serve, None, None, Some(token_for_serve)).await
        });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!(
                "engine never bound socket {}",
                socket_path.display()
            ));
        }

        Ok(Self {
            socket_path,
            token_path,
            pid_path,
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

/// Regression for #720. Pre-fix this `.await` panics inside the
/// nested `block_on` that `stop_engine` used to build; post-fix it
/// completes cleanly.
#[tokio::test]
async fn stop_engine_from_tokio_runtime_completes_via_rpc() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    // Point `default_control_token_path` at our test token file so
    // the shutdown RPC reads the right credential. The env var is
    // the supported override hook for this resolver.
    //
    // SAFETY: This test is the only one in the crate that mutates
    // `BOSS_ENGINE_CONTROL_TOKEN_PATH`. We restore the previous
    // value before returning.
    let prev = std::env::var_os("BOSS_ENGINE_CONTROL_TOKEN_PATH");
    unsafe {
        std::env::set_var("BOSS_ENGINE_CONTROL_TOKEN_PATH", &engine.token_path);
    }

    let pid_path_str = engine.pid_path.to_string_lossy().into_owned();
    let result = boss_client::stop_engine(&pid_path_str).await;

    unsafe {
        match prev {
            Some(v) => std::env::set_var("BOSS_ENGINE_CONTROL_TOKEN_PATH", v),
            None => std::env::remove_var("BOSS_ENGINE_CONTROL_TOKEN_PATH"),
        }
    }

    result.map_err(|e| anyhow!("stop_engine returned Err: {e:#}"))?;

    // The shutdown RPC took the happy path, so the engine should
    // be tearing down its accept loop. The socket goes away within
    // the shutdown_workers grace window + the 50ms response-defer.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut socket_closed = false;
    while std::time::Instant::now() < deadline {
        if !boss_client::engine_socket_reachable(engine.socket_path.to_str().unwrap()).await {
            socket_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        socket_closed,
        "engine should have closed its socket after stop_engine"
    );

    // And the RPC-success branch in stop_engine removes the PID file
    // (since the PID it wrote still matches what we put there).
    assert!(
        !engine.pid_path.exists(),
        "stop_engine should have removed the PID file after a successful RPC shutdown"
    );

    Ok(())
}
