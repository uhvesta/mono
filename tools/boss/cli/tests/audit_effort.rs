//! End-to-end coverage for `boss product audit-effort` — the
//! heuristic feedback-loop audit added by design §Q4 follow-up
//! (PR #370). Spawns an in-process engine on a temp socket, files
//! chores + escalation events via the wire surface the sibling
//! escalation-handler task will use, then drives the `boss` binary
//! through the audit-report path.
//!
//! Acceptance criteria from the work item:
//! - File three escalation events with known markers, run the
//!   report, assert per-marker rates match.
//! - Zero escalations recorded → report says so cleanly, does not
//!   divide by zero.
//! - JSON output exposes the structured report so machine readers
//!   can branch on per-row annotations.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, EffortLevel, FrontendEvent, FrontendRequest, Product, Task, WorkItem,
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
        let work_config = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));
        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });
        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
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

async fn create_product(client: &mut BossClient, name: &str) -> Result<Product> {
    match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput {
                name: name.to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:test/boss.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => Ok(p),
        other => Err(anyhow!("unexpected response for product create: {other:?}")),
    }
}

async fn create_chore(client: &mut BossClient, product_id: &str, name: &str, description: &str) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product_id.to_owned(),
                name: name.to_owned(),
                description: Some(description.to_owned()),
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t) | WorkItem::Chore(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected response for chore create: {other:?}")),
    }
}

async fn record_escalation(
    client: &mut BossClient,
    work_item_id: &str,
    original: EffortLevel,
    new: EffortLevel,
    markers: &[&str],
) -> Result<()> {
    let resp = client
        .send_request(&FrontendRequest::RecordEffortEscalation {
            work_item_id: work_item_id.to_owned(),
            original_level: original,
            new_level: new,
            markers: markers.iter().map(|s| (*s).to_owned()).collect(),
            rule_id: None,
        })
        .await?;
    match resp {
        FrontendEvent::EffortEscalationRecorded { .. } => Ok(()),
        other => Err(anyhow!("unexpected response for record escalation: {other:?}")),
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
    panic!("boss binary path not found");
}

fn run_boss(socket: &str, args: &[&str]) -> Result<Value> {
    let output = Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "boss {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(serde_json::from_str(&String::from_utf8(output.stdout)?)?)
}

fn run_boss_human(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "boss {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Integration smoke test: file three escalation events with known
/// markers, run the audit report, assert the per-marker rates
/// match what was filed. The chore corpus carries one chore per
/// marker we want to test (`rename`, `cursor`, `investigate`) so
/// the denominators are 1 each, making the rates trivial to
/// predict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_escalation_events_match_recorded_rates() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;

    // Three chores, one per marker — denominators all = 1.
    let rename_chore = create_chore(&mut client, &product.id, "Rename auth middleware", "Renames the module").await?;
    let cursor_chore = create_chore(
        &mut client,
        &product.id,
        "Fix cursor flicker",
        "Cursor disappears on focus",
    )
    .await?;
    let _invest_chore = create_chore(
        &mut client,
        &product.id,
        "Investigate the slow path",
        "Diagnose the root cause",
    )
    .await?;

    // Escalate two of three. `investigate` chore does NOT
    // escalate — denominator 1, escalations 0, rate 0%.
    record_escalation(
        &mut client,
        &rename_chore.id,
        EffortLevel::Trivial,
        EffortLevel::Small,
        &["rename"],
    )
    .await?;
    record_escalation(
        &mut client,
        &cursor_chore.id,
        EffortLevel::Trivial,
        EffortLevel::Medium,
        &["cursor"],
    )
    .await?;
    // Third event: same `rename` marker, second escalation. Now
    // rename: denominator 1, escalations 2 → rate is 200%. The
    // report doesn't cap above 100% because that's the truthful
    // ratio of events-to-matches; a >100% rate is itself a signal
    // (the marker fires more often than the corpus suggests).
    record_escalation(
        &mut client,
        &rename_chore.id,
        EffortLevel::Small,
        EffortLevel::Medium,
        &["rename"],
    )
    .await?;

    let report = run_boss(engine.socket_str(), &["product", "audit-effort", &product.id])?;
    let r = &report["report"];
    assert_eq!(r["product_id"].as_str(), Some(product.id.as_str()));
    assert_eq!(r["total_chores"].as_u64(), Some(3));
    assert_eq!(r["total_escalations"].as_u64(), Some(3));

    let rows = r["rows"].as_array().expect("rows array");
    let row_for = |marker: &str| {
        rows.iter()
            .find(|row| row["marker"].as_str() == Some(marker))
            .unwrap_or_else(|| panic!("missing row for marker {marker}"))
    };

    let rename = row_for("rename");
    assert_eq!(rename["matches"].as_u64(), Some(1));
    assert_eq!(rename["escalations"].as_u64(), Some(2));
    assert!((rename["under_class_rate"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(
        rename["annotation"].as_str(),
        Some("consider promoting"),
        "high rate must trigger promote callout"
    );

    let cursor = row_for("cursor");
    assert_eq!(cursor["matches"].as_u64(), Some(1));
    assert_eq!(cursor["escalations"].as_u64(), Some(1));
    assert!((cursor["under_class_rate"].as_f64().unwrap() - 1.0).abs() < 1e-9);

    let investigate = row_for("investigate");
    assert_eq!(investigate["matches"].as_u64(), Some(1));
    assert_eq!(investigate["escalations"].as_u64(), Some(0));
    assert_eq!(investigate["under_class_rate"].as_f64(), Some(0.0));
    // Low volume (1 match) → no "marker holds" callout even at 0%.
    assert!(investigate["annotation"].is_null());
    Ok(())
}

/// Acceptance criterion: empty-data case (no recorded escalations)
/// must emit a clean report without dividing by zero. The
/// `total_escalations` field is 0 and the human renderer prints a
/// "no marker matches yet" line when the chore corpus is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_data_does_not_divide_by_zero() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;

    let report = run_boss(engine.socket_str(), &["product", "audit-effort", &product.id])?;
    let r = &report["report"];
    assert_eq!(r["total_chores"].as_u64(), Some(0));
    assert_eq!(r["total_escalations"].as_u64(), Some(0));
    assert!(r["rows"].as_array().unwrap().is_empty());

    // Human output should not panic / divide by zero either.
    let human = run_boss_human(engine.socket_str(), &["product", "audit-effort", &product.id])?;
    assert!(
        human.contains("0 escalations across 0 chores"),
        "human header missing zero counts: {human}",
    );
    assert!(human.contains("No marker matches"), "human: {human}");
    Ok(())
}

/// A chore matches multiple markers (`cursor` and `apply`); a
/// single escalation event names both. The audit must count the
/// event against each marker — the design's report shape lists
/// per-marker rows independently and double-counting is the
/// expected behaviour for multi-marker matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_marker_event_counts_against_each_marker() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(
        &mut client,
        &product.id,
        "Apply the cursor fix",
        "Apply PR #357 resize-cursor patch",
    )
    .await?;
    record_escalation(
        &mut client,
        &chore.id,
        EffortLevel::Trivial,
        EffortLevel::Small,
        &["apply", "cursor"],
    )
    .await?;
    let report = run_boss(engine.socket_str(), &["product", "audit-effort", &product.id])?;
    let rows = report["report"]["rows"].as_array().unwrap();
    let by_marker = |m: &str| {
        rows.iter()
            .find(|r| r["marker"].as_str() == Some(m))
            .unwrap_or_else(|| panic!("missing {m}"))
    };
    assert_eq!(by_marker("apply")["escalations"].as_u64(), Some(1));
    assert_eq!(by_marker("cursor")["escalations"].as_u64(), Some(1));
    assert_eq!(
        report["report"]["total_escalations"].as_u64(),
        Some(1),
        "total counts the event once even when it names two markers",
    );
    Ok(())
}
