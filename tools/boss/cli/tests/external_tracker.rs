//! End-to-end coverage for `boss product set-external-tracker` and the
//! external-tracker block in `boss product show`.
//!
//! Acceptance criteria:
//! - bind → `product show` renders the tracker block correctly.
//! - unbind → `product show` no longer renders the tracker block.
//! - missing required flags (e.g. no `--org` for `kind=github`) is rejected.
//! - `--json` output round-trips the external_tracker_kind / config fields.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{CreateProductInput, FrontendEvent, FrontendRequest, Product, WorkItem};
use serde_json::Value;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: temp.path().join("state.db"),
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));
        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });
        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }
        Ok(Self { socket_path, _temp: temp, join })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

async fn create_product(client: &mut BossClient, input: CreateProductInput) -> Result<Product> {
    match client.send_request(&FrontendRequest::CreateProduct { input }).await? {
        FrontendEvent::WorkItemCreated { item: WorkItem::Product(p) } => Ok(p),
        other => Err(anyhow!("unexpected event for product create: {other:?}")),
    }
}

fn boss_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_boss") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    if let Ok(runfiles_dir) = std::env::var("RUNFILES_DIR") {
        let p = PathBuf::from(runfiles_dir).join("_main/tools/boss/cli/boss");
        if p.exists() {
            return p;
        }
    }
    panic!("boss binary path not found; ran via cargo or bazel?");
}

fn run_boss(socket: &str, args: &[&str]) -> Result<Value> {
    let output = Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "boss {} failed (status={:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(&stdout)?)
}

fn run_boss_expect_failure(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if output.status.success() {
        return Err(anyhow!(
            "boss {} unexpectedly succeeded: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_then_show_renders_external_tracker_block() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    // Bind the external tracker.
    let bound = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--org",
            "spinyfin",
            "--repo",
            "mono",
            "--project",
            "1",
        ],
    )?;
    assert_eq!(
        bound["product"]["external_tracker_kind"].as_str(),
        Some("github"),
        "bound product should show external_tracker_kind=github: {bound}"
    );
    let config = &bound["product"]["external_tracker_config"];
    assert_eq!(config["org"].as_str(), Some("spinyfin"));
    assert_eq!(config["repo"].as_str(), Some("mono"));
    assert_eq!(config["project_number"].as_u64(), Some(1));
    assert_eq!(config["reverse_close"].as_bool(), Some(false));

    // product show --json should include the same fields.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert_eq!(
        shown["product"]["external_tracker_kind"].as_str(),
        Some("github"),
        "product show should include external_tracker_kind: {shown}"
    );

    // Unbind.
    let unbound = run_boss(
        engine.socket_str(),
        &["product", "set-external-tracker", &product.id, "--unset"],
    )?;
    assert!(
        unbound["product"]["external_tracker_kind"].is_null(),
        "unset should clear external_tracker_kind: {unbound}"
    );

    // product show after unbind should not show the tracker block.
    let after_unset = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert!(
        after_unset["product"]["external_tracker_kind"].is_null(),
        "after unset product show should have no external_tracker_kind: {after_unset}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_with_reverse_close_flag_persists() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "ReverseCloseProd".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    let bound = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--org",
            "spinyfin",
            "--repo",
            "mono",
            "--project",
            "2",
            "--reverse-close",
        ],
    )?;
    let config = &bound["product"]["external_tracker_config"];
    assert_eq!(
        config["reverse_close"].as_bool(),
        Some(true),
        "reverse_close flag should be stored: {config}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_org_for_github_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "NoBind".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    // Missing --org should fail at CLI validation level.
    let err = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--repo",
            "mono",
            "--project",
            "1",
        ],
    )?;
    assert!(
        err.contains("--org") || err.contains("org"),
        "error should mention --org: {err}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_kind_without_unset_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "NoKind".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    let err = run_boss_expect_failure(
        engine.socket_str(),
        &["product", "set-external-tracker", &product.id, "--org", "spinyfin"],
    )?;
    assert!(
        err.contains("--kind") || err.contains("kind") || err.contains("unset"),
        "error should mention --kind or --unset: {err}"
    );

    Ok(())
}
