//! End-to-end coverage for the creation-time repo resolver
//! (`cli/src/repo_resolution.rs`, design §Q4 + follow-up chore #6).
//!
//! Spawns an in-process engine on a temp socket, primes the product
//! with a known-repo set, and drives the `boss` binary through chore
//! create with three signals:
//!
//!   - prompt names a known repo  → resolver picks it (parser step)
//!   - prompt names no known repo → resolver falls through to the
//!     product default
//!   - `--no-input`, product has no default, parser whiffs, no recent
//!     row → resolver errors clearly
//!
//! Together they pin the inference order the way the rest of the
//! system (engine dispatch, app rendering) expects to see it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, FrontendEvent, FrontendRequest, Product, Task,
    WorkItem,
};
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
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!(
                "engine never bound socket {}",
                socket_path.display()
            ));
        }
        Ok(Self {
            socket_path,
            _temp: temp,
            join,
        })
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
    match client
        .send_request(&FrontendRequest::CreateProduct { input })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => Ok(p),
        other => Err(anyhow!(
            "unexpected engine event for product create: {other:?}"
        )),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(task) | WorkItem::Task(task),
        } => Ok(task),
        other => Err(anyhow!("unexpected engine event for chore create: {other:?}")),
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
    Ok(String::from_utf8(output.stderr)?)
}

/// Multi-repo product: prompt names a known repo → resolver picks it.
/// This is the "parser" arm of the Q4 inference chain for products
/// that have no default repo of their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_with_prompt_naming_known_repo_auto_resolves() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    // Product has NO repo (multi-repo product). The known-repo set is
    // bootstrapped from the seed chore's row-level override.
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Work".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
        },
    )
    .await?;
    create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "seed nimbus".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some("git@github.com:foo/nimbus.git".to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "In the nimbus repo, fix the deploy script",
            "--description",
            "",
        ],
    )?;
    assert_eq!(
        created["chore"]["repo_remote_url"].as_str(),
        Some("git@github.com:foo/nimbus.git"),
        "prompt named nimbus → resolver should pick nimbus from the known-repo set"
    );
    Ok(())
}

/// Single-repo product: chore created without --repo stores NULL in
/// `repo_remote_url`. The engine resolves the repo from the product at
/// dispatch time — the row does not need to carry a redundant copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_on_single_repo_product_stores_null_repo_remote_url() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Work".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/console.git".to_owned()),
            design_repo: None,
            docs_repo: None,
        },
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Rewrite the welcome docs",
            "--description",
            "",
        ],
    )?;
    assert!(
        created["chore"]["repo_remote_url"].is_null(),
        "single-repo product: task row must store NULL, not the product's URL; got: {}",
        created["chore"]["repo_remote_url"]
    );
    Ok(())
}

/// Acceptance for the "no-input + no resolution" error path. Product
/// has no default, no recent override, prompt mentions no known repo,
/// `--no-input` is on → refuse with an actionable message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_no_input_with_no_resolution_errors_clearly() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Greenfield".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
        },
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "do a thing",
            "--description",
            "",
        ],
    )?;
    assert!(
        stderr.contains("could not resolve repo"),
        "stderr should explain the resolver whiffed: {stderr}"
    );
    assert!(
        stderr.contains("--repo"),
        "stderr should point at the --repo flag: {stderr}"
    );
    assert!(
        stderr.contains(&product.slug) || stderr.contains("product update"),
        "stderr should mention the product or the `product update` remedy: {stderr}"
    );
    Ok(())
}

/// `--repo` on a single-repo product is rejected with a clear error.
/// Products that have their own `repo_remote_url` do not allow per-task
/// overrides; the error message names the product and its repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_explicit_repo_rejected_on_single_repo_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Work".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/console.git".to_owned()),
            design_repo: None,
            docs_repo: None,
        },
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "do a thing",
            "--description",
            "",
            "--repo",
            "git@github.com:foo/other.git",
        ],
    )?;
    assert!(
        stderr.contains("cannot set per-task repo override"),
        "stderr should explain the override is not allowed: {stderr}"
    );
    assert!(
        stderr.contains(&product.slug),
        "stderr should name the product: {stderr}"
    );
    assert!(
        stderr.contains("console.git"),
        "stderr should name the product's repo: {stderr}"
    );
    Ok(())
}

/// `--repo` on a multi-repo product (no product default) is accepted and
/// stored in the task row. This is the legitimate per-task override path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_explicit_repo_accepted_on_multi_repo_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Greenfield".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
        },
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "bootstrap the new service",
            "--description",
            "",
            "--repo",
            "git@github.com:foo/new-service.git",
        ],
    )?;
    assert_eq!(
        created["chore"]["repo_remote_url"].as_str(),
        Some("git@github.com:foo/new-service.git"),
        "multi-repo product: explicit --repo should be stored in the row"
    );
    Ok(())
}
