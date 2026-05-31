//! Integration tests for the token-authenticated `Shutdown` RPC.
//!
//! Issue #705: `SIGTERM` had no way to distinguish "the macOS app is
//! auto-restarting me" from "a worker test accidentally targeted
//! `/tmp/boss-engine.pid`". The token gate fixes that by making the
//! everyday shutdown path require a credential that lives at a path
//! the bazel sandbox already denies access to.
//!
//! These tests exercise both halves on a real engine bound to a
//! temp socket:
//!   - a valid token is accepted, the engine sends
//!     `ShutdownAccepted`, and the accept loop exits.
//!   - a wrong token is rejected with `ShutdownRejected { reason:
//!     "token_mismatch" }` and the engine keeps running.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::engine_control::ControlTokenFile;
use boss_protocol::{FrontendEvent, FrontendRequest};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    token_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let token_path = temp.path().join("engine-control.token");
        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: db_path.clone(),
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let token_for_serve = token_path.clone();
        let join = tokio::spawn(async move {
            serve(cfg, socket_for_serve, None, None, Some(token_for_serve), None).await
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
            _temp: temp,
            join,
        })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }

    fn read_token(&self) -> Result<ControlTokenFile> {
        let raw = std::fs::read_to_string(&self.token_path)?;
        let parsed: ControlTokenFile = serde_json::from_str(&raw)?;
        Ok(parsed)
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

#[tokio::test]
async fn shutdown_with_correct_token_is_accepted() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    // Token file must exist and contain the canonical schema.
    let parsed = engine
        .read_token()
        .map_err(|e| anyhow!("failed to read token file: {e}"))?;
    assert_eq!(parsed.token.len(), 64, "token should be 64 hex chars");
    assert_eq!(
        parsed.socket_path,
        engine.socket_path.display().to_string()
    );

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::Shutdown {
            token: parsed.token.clone(),
        })
        .await?;
    match response {
        FrontendEvent::ShutdownAccepted => {}
        other => return Err(anyhow!("expected ShutdownAccepted, got {other:?}")),
    }

    // The engine should now exit its accept loop within the
    // shutdown_workers grace window (5s) + the 50ms response-defer.
    // Probe by trying to reconnect — connection refused means the
    // socket closed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut socket_closed = false;
    while std::time::Instant::now() < deadline {
        if !boss_client::engine_socket_reachable(engine.socket_str()).await {
            socket_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        socket_closed,
        "engine should have closed its socket after ShutdownAccepted"
    );

    Ok(())
}

#[tokio::test]
async fn shutdown_with_wrong_token_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::Shutdown {
            token: "not-the-real-token".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::ShutdownRejected { reason } => {
            assert_eq!(reason, "token_mismatch");
        }
        other => return Err(anyhow!("expected ShutdownRejected, got {other:?}")),
    }

    // The engine must still be alive — a second request should
    // succeed.
    let v = client
        .send_request(&FrontendRequest::GetEngineVersion)
        .await?;
    match v {
        FrontendEvent::EngineVersionResult { .. } => Ok(()),
        other => Err(anyhow!(
            "engine should still respond after a rejected shutdown; got {other:?}"
        )),
    }
}
