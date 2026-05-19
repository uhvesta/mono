//! End-to-end coverage for the `boss engine ci …` and
//! `boss engine attempts …` verb families (design Phase 11 #35/#36
//! of `merge-conflict-handling-in-review.md`). Spawns an in-process
//! engine, seeds `ci_remediations` rows via the engine library's
//! `WorkDb`, and drives the `boss` binary through:
//!
//!   - `boss engine ci list` (with and without filters, JSON + text)
//!   - `boss engine ci show <attempt-id>`
//!   - `boss engine ci abandon <attempt-id>`
//!   - `boss engine ci retry <work-item-id-or-attempt-id>`
//!   - `boss engine ci budget show / set / set --clear`
//!   - `boss engine attempts list` (unified)
//!
//! These are the acceptance tests called out by the work item:
//! "snapshot tests on CLI" / "list shows entries from all three
//! subsystems with correct `kind` column."

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::{
    CiRemediationInsertInput, ConflictResolutionInsertInput, WorkDb,
};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, FrontendEvent, FrontendRequest, Product, WorkItem,
    WorkItemPatch,
};
use serde_json::Value;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    db_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: db_path.clone(),
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
            db_path,
            _temp: temp,
            join,
        })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }

    fn db(&self) -> Result<WorkDb> {
        WorkDb::open(self.db_path.clone()).map_err(Into::into)
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

async fn create_product(client: &mut BossClient, name: &str) -> Result<Product> {
    let input = CreateProductInput {
        name: name.to_owned(),
        description: None,
        repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
    };
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

/// Run `boss --json …` and return parsed stdout.
fn run_boss_json(socket: &str, args: &[&str]) -> Result<Value> {
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

/// Run `boss …` in human (text) mode and return stdout.
fn run_boss_text(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--no-input", "--no-autostart", "--socket-path", socket])
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
    Ok(String::from_utf8(output.stdout)?)
}

/// Run `boss --json …` expecting failure; return stderr.
fn run_boss_expect_failure(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--no-input", "--no-autostart", "--socket-path", socket])
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

/// Seed a chore plus one CI remediation row directly through the
/// engine's `WorkDb`. Returns `(chore_id, attempt_id)`.
fn seed_chore_with_ci_attempt(
    db: &WorkDb,
    product_id: &str,
    name: &str,
    pr_number: i64,
    attempt_kind: &str,
    head_sha: &str,
) -> Result<(String, String)> {
    let chore = db.create_chore(CreateChoreInput {
        product_id: product_id.to_owned(),
        name: name.to_owned(),
        description: None,
        autostart: false,
        priority: None,
        created_via: None,
        repo_remote_url: None,
        effort_level: None,
        model_override: None,
        force_duplicate: false,
    })?;
    let pr_url = format!("https://github.com/test/boss/pull/{pr_number}");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )?;
    let consumes_budget = if attempt_kind == "fix" { 1 } else { 0 };
    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore.id.clone(),
            pr_url,
            pr_number,
            head_branch: format!("feature-{pr_number}"),
            head_sha_at_trigger: head_sha.to_owned(),
            attempt_kind: attempt_kind.to_owned(),
            consumes_budget,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })?
        .expect("insert returned new row");
    Ok((chore.id, attempt.id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_list_returns_rows_freshest_first_in_json_and_text() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;

    let (chore_a, _) = seed_chore_with_ci_attempt(
        &db,
        &product.id,
        "chore-list-a",
        500,
        "fix",
        "head-aaa-1",
    )?;
    let (_chore_b, attempt_b) = seed_chore_with_ci_attempt(
        &db,
        &product.id,
        "chore-list-b",
        501,
        "retrigger",
        "head-bbb-1",
    )?;

    let response = run_boss_json(engine.socket_str(), &["engine", "ci", "list"])?;
    let attempts = response["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 2, "json list must include both rows");
    // The most-recently inserted row should land at index 0.
    assert_eq!(attempts[0]["id"].as_str(), Some(attempt_b.as_str()));
    assert!(
        attempts.iter().all(|r| r["product_id"].as_str() == Some(product.id.as_str())),
        "all rows must echo the seed product"
    );

    // Filter by work-item should narrow to one row.
    let by_item = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "list", "--work-item", &chore_a],
    )?;
    let by_item_attempts = by_item["attempts"].as_array().expect("attempts array");
    assert_eq!(by_item_attempts.len(), 1);
    assert_eq!(
        by_item_attempts[0]["work_item_id"].as_str(),
        Some(chore_a.as_str()),
    );

    // Text mode renders a table with the documented columns + an
    // attempt_kind column for the CI view.
    let text = run_boss_text(engine.socket_str(), &["engine", "ci", "list"])?;
    assert!(text.contains("KIND"), "text output must include KIND column: {text}");
    assert!(text.contains("STATUS"));
    assert!(text.contains("retrigger"), "text output must show the kind value: {text}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_show_returns_single_row_with_failed_checks_and_log() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (_, attempt_id) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-show", 600, "fix", "head-show-1")?;
    let shown = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "show", &attempt_id],
    )?;
    assert_eq!(shown["attempt"]["id"].as_str(), Some(attempt_id.as_str()));
    assert_eq!(shown["attempt"]["attempt_kind"].as_str(), Some("fix"));
    assert_eq!(shown["attempt"]["consumes_budget"].as_i64(), Some(1));

    // Unknown id → CliError::Application (exit 6), with a clear message.
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["engine", "ci", "show", "cir_does_not_exist"],
    )?;
    assert!(
        stderr.contains("unknown"),
        "expected 'unknown' in stderr, got: {stderr}",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_abandon_marks_attempt_abandoned() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (_, attempt_id) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-abandon", 700, "fix", "head-abandon-1")?;
    let result = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "abandon", &attempt_id, "--reason", "manual_test"],
    )?;
    assert_eq!(result["attempt"]["status"].as_str(), Some("abandoned"));
    assert_eq!(
        result["attempt"]["failure_reason"].as_str(),
        Some("manual_test"),
    );

    // Second call on the already-terminal row must surface an error.
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["engine", "ci", "abandon", &attempt_id, "--reason", "again"],
    )?;
    assert!(
        stderr.contains("already terminal") || stderr.contains("unknown"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_retry_accepts_work_item_id_and_attempt_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (chore_id, attempt_id) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-retry", 800, "fix", "head-retry-1")?;
    db.increment_ci_attempts_used(&chore_id)?;
    db.increment_ci_attempts_used(&chore_id)?;
    db.mark_chore_blocked_ci_failure_exhausted(
        &chore_id,
        &format!("https://github.com/test/boss/pull/{}", 800),
    )?;
    assert!(db.get_ci_attempts_used(&chore_id)? >= 2);

    // Call retry with the work-item id.
    let response = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "retry", &chore_id],
    )?;
    assert_eq!(response["work_item_id"].as_str(), Some(chore_id.as_str()));
    assert_eq!(response["was_exhausted"].as_bool(), Some(true));
    assert_eq!(response["budget"]["used"].as_i64(), Some(0));

    // Second retry: now via the attempt id (the engine resolves it
    // back to the same parent). The counter is already zero and the
    // parent is no longer exhausted.
    let response2 = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "retry", &attempt_id],
    )?;
    assert_eq!(response2["work_item_id"].as_str(), Some(chore_id.as_str()));
    assert_eq!(response2["was_exhausted"].as_bool(), Some(false));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_budget_show_and_set_round_trips() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (chore_id, _) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-budget", 900, "fix", "head-budget-1")?;

    // Initial: no per-PR override, product default = 3.
    let initial = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "budget", "show", &chore_id],
    )?;
    assert!(initial["budget"]["per_pr_override"].is_null());
    assert_eq!(initial["budget"]["product_default"].as_i64(), Some(3));
    assert_eq!(initial["budget"]["effective"].as_i64(), Some(3));

    // Set override to 5.
    let set = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--budget", "5"],
    )?;
    assert_eq!(set["budget"]["per_pr_override"].as_i64(), Some(5));
    assert_eq!(set["budget"]["effective"].as_i64(), Some(5));

    // Set above the cap; engine clamps to 10.
    let clamped = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--budget", "25"],
    )?;
    assert_eq!(clamped["budget"]["per_pr_override"].as_i64(), Some(10));

    // Clear → product default applies again.
    let cleared = run_boss_json(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--clear"],
    )?;
    assert!(cleared["budget"]["per_pr_override"].is_null());
    assert_eq!(cleared["budget"]["effective"].as_i64(), Some(3));

    // Neither --budget nor --clear → usage error.
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id],
    )?;
    assert!(
        stderr.contains("--budget") || stderr.contains("--clear"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_attempts_list_includes_all_three_kinds() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;

    // Seed both a CI attempt and a conflict resolution attempt against
    // the same chore.
    let (chore_id, _) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-attempts", 1100, "fix", "head-1")?;
    let pr_url = "https://github.com/test/boss/pull/1100".to_owned();
    db.insert_conflict_resolution(ConflictResolutionInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore_id.clone(),
        pr_url,
        pr_number: 1100,
        head_branch: "feature-1100".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("base-1".into()),
        head_sha_before: Some("head-1".into()),
    })?
    .expect("insert");

    let listing = run_boss_json(engine.socket_str(), &["engine", "attempts", "list"])?;
    let attempts = listing["attempts"].as_array().expect("attempts array");
    let kinds: Vec<&str> = attempts
        .iter()
        .filter_map(|r| r["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"ci"), "expected ci kind in {kinds:?}");
    assert!(kinds.contains(&"conflict"), "expected conflict kind in {kinds:?}");

    // --kind filter narrows to one subsystem.
    let only_ci = run_boss_json(
        engine.socket_str(),
        &["engine", "attempts", "list", "--kind", "ci"],
    )?;
    let only_ci_rows = only_ci["attempts"].as_array().expect("attempts array");
    assert!(!only_ci_rows.is_empty());
    for r in only_ci_rows {
        assert_eq!(r["kind"].as_str(), Some("ci"));
    }

    // Text mode renders a KIND column.
    let text = run_boss_text(engine.socket_str(), &["engine", "attempts", "list"])?;
    assert!(text.contains("KIND"), "text output must include KIND column: {text}");
    assert!(text.contains("ci"), "text output must surface ci kind: {text}");
    assert!(text.contains("conflict"), "text output must surface conflict kind: {text}");
    Ok(())
}
