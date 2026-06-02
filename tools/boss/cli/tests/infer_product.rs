//! End-to-end coverage for `boss` CLI product inference. Spawns an
//! in-process engine on a temp socket, sets up a product / project /
//! task via `boss-client`, then drives the `boss` binary with typed
//! ids (no `--product`) and checks the response.
//!
//! The fix this exercises: `boss project show proj_…` and
//! `boss task list --project proj_…` previously errored with
//! "product is required" — globally-unique typed ids are enough to
//! locate the product, and the CLI now infers it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CreateProductInput, CreateProjectInput, CreateTaskInput, FrontendEvent, FrontendRequest,
    Product, Project, Task, WorkItem,
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
        let work_config = WorkConfig::builder().cwd(temp.path().to_path_buf()).db_path(temp.path().join("state.db")).build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });

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
    match client.send_request(&FrontendRequest::CreateProduct { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => Ok(p),
        other => Err(anyhow!("unexpected engine event for product create: {other:?}")),
    }
}

async fn create_project(client: &mut BossClient, input: CreateProjectInput) -> Result<Project> {
    match client.send_request(&FrontendRequest::CreateProject { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Project(p),
        } => Ok(p),
        other => Err(anyhow!("unexpected engine event for project create: {other:?}")),
    }
}

async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task> {
    match client.send_request(&FrontendRequest::CreateTask { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected engine event for task create: {other:?}")),
    }
}

/// Resolve the `boss` binary path. Cargo defines `CARGO_BIN_EXE_boss`
/// for integration tests automatically. Under Bazel the rust_test rule
/// stages the binary as a data dep and we resolve it through
/// `RUNFILES_DIR` (set by `rust_test`'s test runner). Falling back to
/// `$PATH` would silently hit whatever stale binary the user has
/// installed system-wide, so we panic if neither path resolves.
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

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_infers_product_from_typed_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;

    // The bug: `project show proj_…` errored with
    // "product is required" even though the id is globally unique.
    let value = run_boss(engine.socket_str(), &["project", "show", &project.id])?;
    assert_eq!(
        value["project"]["id"].as_str(),
        Some(project.id.as_str()),
    );
    assert_eq!(
        value["project"]["product_id"].as_str(),
        Some(product.id.as_str()),
    );
    Ok(())
}

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_infers_product_from_project_typed_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;
    let task = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "wire it up".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;

    let value = run_boss(
        engine.socket_str(),
        &["task", "list", "--project", &project.id],
    )?;
    let tasks = value["tasks"]
        .as_array()
        .ok_or_else(|| anyhow!("expected `tasks` array in CLI output: {value}"))?;
    // Two rows: the auto-created design task plus the explicit one
    // we just inserted. Both belong to the inferred product.
    assert!(tasks.iter().any(|t| t["id"].as_str() == Some(&task.id)));
    assert!(tasks.iter().all(|t| t["product_id"].as_str() == Some(&product.id)));
    Ok(())
}

/// When the user supplies both `--product` and a typed id whose
/// product disagrees, the CLI must refuse instead of silently
/// favouring one side. The error names both sides so the redundant
/// `--product` is easy to drop.
// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_rejects_disagreeing_explicit_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let primary = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let other = create_product(
        &mut client,
        CreateProductInput {
            name: "Mono".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: primary.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;

    let output = Command::new(boss_binary())
        .args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
            "project",
            "show",
            "--product",
            &other.slug,
            &project.id,
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "mismatch must exit non-zero, stdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&project.id) && stderr.contains(&primary.id),
        "error must name both products: {stderr}"
    );
    Ok(())
}
