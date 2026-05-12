//! End-to-end coverage for the effort-level / model-override CLI
//! surface added by PR #370's schema task. Spawns an in-process
//! engine on a temp socket, drives the `boss` binary through the
//! create / edit / show paths, and checks the new fields land in
//! both the JSON response shape and the round-tripped DB row.
//!
//! Acceptance criteria from the work item:
//! - `boss chore create --effort large --model claude-opus-4-7 …`
//!   succeeds and `boss chore show <id> --json` returns both fields.
//! - `boss chore create --effort galaxybrain` fails fast.
//! - `boss product set-default-model boss --model sonnet` succeeds
//!   and `boss product show boss --json` includes
//!   `default_model: "sonnet"`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CreateProductInput, FrontendEvent, FrontendRequest, Product, WorkItem,
};
use serde_json::Value;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

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
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None).await });

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
        other => Err(anyhow!(
            "unexpected engine event for product create: {other:?}"
        )),
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

/// Run `boss --json …` and return parsed stdout. Mirrors the helper
/// in `infer_product.rs` so the two test files stay self-contained.
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

/// Run `boss --json …` expecting failure; return stderr.
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

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. The in-process
// engine's accept loop needs a separate worker thread to handle
// connects while the test thread is parked on the subprocess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_with_effort_and_model_round_trips_through_show() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
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
            "Investigate the slow path",
            "--description",
            "ought to take all day",
            "--effort",
            "large",
            "--model",
            "claude-opus-4-7",
        ],
    )?;
    let chore_id = created["chore"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("chore create did not return an id: {created}"))?
        .to_owned();
    assert_eq!(created["chore"]["effort_level"].as_str(), Some("large"));
    assert_eq!(
        created["chore"]["model_override"].as_str(),
        Some("claude-opus-4-7"),
    );

    let shown = run_boss(engine.socket_str(), &["chore", "show", &chore_id])?;
    assert_eq!(shown["chore"]["effort_level"].as_str(), Some("large"));
    assert_eq!(
        shown["chore"]["model_override"].as_str(),
        Some("claude-opus-4-7"),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_rejects_invalid_effort_level() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
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
            "fix it",
            "--description",
            "",
            "--effort",
            "galaxybrain",
        ],
    )?;
    // clap surfaces the allowed-values set in its error message; the
    // five values from the design's Q1 enum are the contract we ship.
    assert!(stderr.contains("galaxybrain"), "stderr: {stderr}");
    assert!(stderr.contains("trivial"), "stderr: {stderr}");
    assert!(stderr.contains("max"), "stderr: {stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_update_sets_clears_effort_and_model_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
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
            "Some work",
            "--description",
            "",
        ],
    )?;
    let chore_id = created["chore"]["id"]
        .as_str()
        .expect("chore id")
        .to_owned();
    // Fresh row: NULL on both fields.
    assert!(created["chore"]["effort_level"].is_null());
    assert!(created["chore"]["model_override"].is_null());

    // Set via update.
    let updated = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore_id,
            "--effort",
            "medium",
            "--model",
            "sonnet",
        ],
    )?;
    assert_eq!(updated["chore"]["effort_level"].as_str(), Some("medium"));
    assert_eq!(updated["chore"]["model_override"].as_str(), Some("sonnet"));

    // Clear via --unset-effort / --unset-model.
    let cleared = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore_id,
            "--unset-effort",
            "--unset-model",
        ],
    )?;
    assert!(cleared["chore"]["effort_level"].is_null());
    assert!(cleared["chore"]["model_override"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn product_set_default_model_lifecycle_round_trips() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
        },
    )
    .await?;

    // Fresh row: no default model.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert!(shown["product"]["default_model"].is_null());

    // Set.
    let set_resp = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-default-model",
            &product.id,
            "--model",
            "sonnet",
        ],
    )?;
    assert_eq!(
        set_resp["product"]["default_model"].as_str(),
        Some("sonnet"),
    );

    // Show reflects it.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert_eq!(shown["product"]["default_model"].as_str(), Some("sonnet"));

    // Clear.
    let unset_resp = run_boss(
        engine.socket_str(),
        &["product", "set-default-model", &product.id, "--unset"],
    )?;
    assert!(unset_resp["product"]["default_model"].is_null());
    Ok(())
}

/// Slugs are stored verbatim — the engine deliberately does not
/// validate against any known set. Confirms a hypothetical future
/// model slug round-trips today without an engine release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_override_passes_through_unrecognised_slug() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
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
            "n",
            "--description",
            "",
            "--model",
            "claude-future-model-7-extra-thinking",
        ],
    )?;
    assert_eq!(
        created["chore"]["model_override"].as_str(),
        Some("claude-future-model-7-extra-thinking"),
    );
    Ok(())
}
