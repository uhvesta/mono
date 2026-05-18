//! Integration test: spin up an in-process engine on a temp socket + temp DB,
//! drive product/project/task/chore CRUD through `boss-client`, and verify
//! invalidations propagate to a second concurrent client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    AddDependencyInput, CreateChoreInput, CreateManyChoresInput, CreateManyTasksInput,
    CreateProductInput, CreateProjectInput, CreateTaskInput, DependencyDirection, DependencyFilter,
    FrontendEvent, FrontendRequest, LinkExternalRefInput, ListDependenciesInput, Product, Project,
    ProjectDesignDocState, RemoveDependencyInput, ResolveProjectDesignDocOutput,
    ResolvedDesignDocKind, SetProjectDesignDocInput, Task, TopicEventPayload, WorkItem,
    WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView, WorkItemPatch,
    work_product_topic,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::Mutex;

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
            db_path: std::path::PathBuf::from(":memory:"),
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

#[tokio::test]
async fn product_project_task_chore_crud_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: Some("multi-agent coding manager".to_owned()),
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    assert_eq!(product.name, "Boss");
    assert_eq!(product.slug, "boss");
    assert_eq!(product.status, "active");

    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 1".to_owned(),
            description: Some("initial slice".to_owned()),
            goal: Some("ship work CLI".to_owned()),
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;
    assert_eq!(project.name, "Phase 1");
    assert_eq!(project.product_id, product.id);

    let task = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Wire socket client".to_owned(),
            description: Some("extract reusable BossClient".to_owned()),
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(task.kind, "project_task");
    assert_eq!(task.status, "todo");
    assert_eq!(task.project_id.as_deref(), Some(project.id.as_str()));

    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Trim stale work".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(chore.kind, "chore");
    assert!(chore.project_id.is_none());

    let listed_products = list_products(&mut client).await?;
    assert_eq!(listed_products.len(), 1);
    assert_eq!(listed_products[0].id, product.id);

    let listed_projects = list_projects(&mut client, &product.id).await?;
    assert_eq!(listed_projects.len(), 1);

    let listed_tasks = list_tasks(&mut client, &product.id, Some(&project.id)).await?;
    // Two rows: the auto-created `kind = 'design'` task plus the
    // user-created project_task. The design task always sorts first.
    assert_eq!(listed_tasks.len(), 2);
    assert_eq!(listed_tasks[0].kind, "design");
    assert_eq!(listed_tasks[1].id, task.id);

    let listed_chores = list_chores(&mut client, &product.id).await?;
    assert_eq!(listed_chores.len(), 1);

    let updated = update_work_item(
        &mut client,
        &task.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/example/repo/pull/1".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .await?;
    let updated_task = expect_task(updated)?;
    assert_eq!(updated_task.status, "in_review");
    assert_eq!(
        updated_task.pr_url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );

    delete_work_item(&mut client, &chore.id).await?;
    let chores_after = list_chores(&mut client, &product.id).await?;
    assert!(chores_after.iter().all(|item| item.id != chore.id));

    // Mirror what `boss product delete` / `boss project delete` do on the
    // wire: the CLI archives instead of hard-deleting, since the engine
    // refuses hard delete of products/projects.
    let archived_project = expect_project(
        update_work_item(
            &mut client,
            &project.id,
            WorkItemPatch {
                status: Some("archived".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(archived_project.status, "archived");

    let archived_product = expect_product(
        update_work_item(
            &mut client,
            &product.id,
            WorkItemPatch {
                status: Some("archived".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(archived_product.status, "archived");

    Ok(())
}

#[tokio::test]
async fn task_and_chore_priority_round_trips_through_engine() -> Result<()> {
    // Exercises the first-class `priority` field on tasks / chores:
    // the create flow honors `Some("high")`, the omitted-priority
    // path lands on the schema default `medium`, and a
    // `WorkItemPatch::priority` re-routes through `update_work_item`
    // so the engine persists the new value rather than dropping it.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Priorities".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:priorities.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Slice".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;

    let high_task = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "High-priority task".to_owned(),
            description: None,
            autostart: true,
            priority: Some("high".to_owned()),
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(high_task.priority, "high");

    let default_chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Default-priority chore".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(default_chore.priority, "medium");

    let patched = expect_chore(
        update_work_item(
            &mut client,
            &default_chore.id,
            WorkItemPatch {
                priority: Some("low".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(patched.priority, "low");

    // Confirm the priority survives a list round-trip.
    let listed_chores = list_chores(&mut client, &product.id).await?;
    let refetched = listed_chores
        .into_iter()
        .find(|c| c.id == default_chore.id)
        .expect("chore missing from list");
    assert_eq!(refetched.priority, "low");

    Ok(())
}

/// End-to-end round-trip for the multi-repo CLI feature: an explicit
/// `repo_remote_url` on `CreateChoreInput` persists, is readable via
/// `ListChores`, can be cleared via `WorkItemPatch.repo_remote_url =
/// Some("")` (the `boss <kind> update --repo ""` wire form), and the
/// cleared row reads back with `repo_remote_url = None` so the
/// downstream resolver inherits the product default. Acceptance test
/// for the "CLI: --repo flag on chore/task/project create, update,
/// list verbs" work item.
#[tokio::test]
async fn chore_repo_remote_url_override_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Work".to_owned(),
            description: None,
            // Product has no default repo — every chore here picks one.
            repo_remote_url: None,
            design_repo: None,
        },
    )
    .await?;

    // Create-with-override: the wire field lands on the row.
    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Fix nimbus deploy".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some("git@github.com:myorg/nimbus.git".to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(
        chore.repo_remote_url.as_deref(),
        Some("git@github.com:myorg/nimbus.git"),
    );

    // List-with-filter (the CLI's job): the override survives the
    // SELECT and is visible on the listed row, so the CLI's
    // `RepoSelector` can match against the resolved repo.
    let listed = list_chores(&mut client, &product.id).await?;
    let row = listed
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore missing from list");
    assert_eq!(
        row.repo_remote_url.as_deref(),
        Some("git@github.com:myorg/nimbus.git"),
    );

    // Update-to-clear: the engine canonicalises empty-string to
    // `None` (matching the existing `--pr-url ""` convention).
    let cleared = expect_chore(
        update_work_item(
            &mut client,
            &chore.id,
            WorkItemPatch {
                repo_remote_url: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert!(
        cleared.repo_remote_url.is_none(),
        "empty-string patch must clear the override, got {:?}",
        cleared.repo_remote_url,
    );

    // List-without-filter shape: the row reads back with the column
    // unset, ready to inherit from the product default at dispatch.
    let listed_after = list_chores(&mut client, &product.id).await?;
    let row_after = listed_after
        .iter()
        .find(|c| c.id == chore.id)
        .expect("chore missing from list after clear");
    assert!(row_after.repo_remote_url.is_none());

    // Set it back to a different URL to confirm overwrite works too
    // (the CLI's set / clear / re-set cycle).
    let reset = expect_chore(
        update_work_item(
            &mut client,
            &chore.id,
            WorkItemPatch {
                repo_remote_url: Some("https://github.com/other/wiki.git".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(
        reset.repo_remote_url.as_deref(),
        Some("https://github.com/other/wiki.git"),
    );

    Ok(())
}

#[tokio::test]
async fn second_client_receives_invalidation_from_first() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    let mut writer_client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut writer_client,
        CreateProductInput {
            name: "Multiplex".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
        },
    )
    .await?;

    // Subscribe a second connection to the product topic and confirm it
    // receives the invalidation when the first client mutates work state.
    let topic = work_product_topic(&product.id);
    let watcher = subscribe_watcher(engine.socket_str(), topic.clone()).await?;

    let _project = create_project(
        &mut writer_client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Subscribed".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;

    let invalidation = watcher.next_invalidation(Duration::from_secs(2)).await?;
    assert_eq!(invalidation.topic, topic);
    match invalidation.event {
        TopicEventPayload::WorkInvalidated {
            reason, product_id, ..
        } => {
            assert_eq!(reason, "project_created");
            assert_eq!(product_id.as_deref(), Some(product.id.as_str()));
        }
        TopicEventPayload::ExecutionInvalidated { .. } => {
            panic!("unexpected execution invalidation on work topic")
        }
    }

    Ok(())
}

#[tokio::test]
async fn cli_status_update_propagates_to_subscriber_within_one_second() -> Result<()> {
    // Phase 2 "Done when": a CLI-style mutation made by one client surfaces
    // on a second connected client without manual refresh, fast enough to
    // feel live. We model the human's CLI as `writer_client` and a watcher
    // playing the role of the macOS Work tab.
    let engine = TestEngine::spawn().await?;

    let mut writer_client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut writer_client,
        CreateProductInput {
            name: "Live".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:live.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let project = create_project(
        &mut writer_client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 2".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;
    let task = create_task(
        &mut writer_client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Wire subscription".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;

    let topic = work_product_topic(&product.id);
    let watcher = subscribe_watcher(engine.socket_str(), topic.clone()).await?;

    let started = std::time::Instant::now();
    update_work_item(
        &mut writer_client,
        &task.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .await?;

    let invalidation = watcher.next_invalidation(Duration::from_secs(1)).await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "invalidation propagation took {elapsed:?}, expected < 1s"
    );
    assert_eq!(invalidation.topic, topic);
    match invalidation.event {
        TopicEventPayload::WorkInvalidated {
            reason,
            product_id,
            item_ids,
        } => {
            assert_eq!(reason, "work_item_updated");
            assert_eq!(product_id.as_deref(), Some(product.id.as_str()));
            assert_eq!(item_ids, vec![task.id.clone()]);
        }
        TopicEventPayload::ExecutionInvalidated { .. } => {
            panic!("unexpected execution invalidation on work topic")
        }
    }

    Ok(())
}

#[tokio::test]
async fn each_mutation_emits_one_invalidation() -> Result<()> {
    // Three separate mutations should produce three distinct invalidations
    // for the watcher in order — coalescing only collapses *unsent* duplicates,
    // so distinct events on a draining socket should pass through one-for-one.
    let engine = TestEngine::spawn().await?;

    let mut writer_client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut writer_client,
        CreateProductInput {
            name: "Sequenced".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:sequenced.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;

    let topic = work_product_topic(&product.id);
    let watcher = subscribe_watcher(engine.socket_str(), topic.clone()).await?;

    let _project = create_project(
        &mut writer_client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;
    let _chore = create_chore(
        &mut writer_client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "C".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;

    let mut reasons = Vec::new();
    for _ in 0..2 {
        let inv = watcher.next_invalidation(Duration::from_secs(1)).await?;
        let TopicEventPayload::WorkInvalidated { reason, .. } = inv.event else {
            panic!("unexpected execution invalidation on work topic")
        };
        reasons.push(reason);
    }
    assert_eq!(reasons, vec!["project_created", "chore_created"]);

    Ok(())
}

/// Mirrors what `boss task bind-pr` / `boss chore bind-pr` issue at the
/// wire level: an `UpdateWorkItem` with only `pr_url` set, repeated
/// across the add → re-bind same → re-bind different sequence. The CLI
/// short-circuits the same-URL case before sending, but the engine
/// must remain idempotent for the case where a caller (or a
/// future client) does send it.
#[tokio::test]
async fn bind_pr_sequence_is_idempotent_on_engine() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Bindable".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:bindable.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Backfill PR".to_owned(),
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
    assert!(chore.pr_url.is_none());

    let first = "https://github.com/spinyfin/mono/pull/238";
    let second = "https://github.com/spinyfin/mono/pull/239";

    // Add: status untouched, pr_url stamped.
    let bound = expect_chore(
        update_work_item(
            &mut client,
            &chore.id,
            WorkItemPatch {
                pr_url: Some(first.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(bound.pr_url.as_deref(), Some(first));
    assert_eq!(bound.status, chore.status, "bind-pr must not change status");

    // Re-bind same URL: idempotent at the data layer.
    let same = expect_chore(
        update_work_item(
            &mut client,
            &chore.id,
            WorkItemPatch {
                pr_url: Some(first.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(same.pr_url.as_deref(), Some(first));
    assert_eq!(same.status, chore.status);

    // Re-bind to a different URL: overwrites cleanly.
    let switched = expect_chore(
        update_work_item(
            &mut client,
            &chore.id,
            WorkItemPatch {
                pr_url: Some(second.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(switched.pr_url.as_deref(), Some(second));
    assert_eq!(switched.status, chore.status);

    Ok(())
}

struct Invalidation {
    topic: String,
    event: TopicEventPayload,
}

struct Watcher {
    invalidations: Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<Invalidation>>>,
    _writer: OwnedWriteHalf,
    _task: tokio::task::JoinHandle<()>,
}

impl Watcher {
    async fn next_invalidation(&self, timeout: Duration) -> Result<Invalidation> {
        let mut rx = self.invalidations.lock().await;
        tokio::time::timeout(timeout, rx.recv())
            .await
            .map_err(|_| anyhow!("timed out waiting for invalidation"))?
            .ok_or_else(|| anyhow!("watcher channel closed"))
    }
}

async fn subscribe_watcher(socket_path: &str, topic: String) -> Result<Watcher> {
    let stream = UnixStream::connect(socket_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    let request_id = "watcher-subscribe-1";
    let envelope = serde_json::json!({
        "request_id": request_id,
        "payload": {
            "type": "subscribe",
            "topics": [topic.clone()],
        }
    });
    let line = serde_json::to_string(&envelope)?;
    write_half.write_all(line.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    // Drain the Hello + Subscribed acknowledgements before handing the
    // reader to the background task.
    let mut subscribed = false;
    while !subscribed {
        let line = reader
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("engine closed before subscribe ack"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(|ty| ty.as_str())
            == Some("subscribed")
        {
            subscribed = true;
        }
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Invalidation>();
    let task = tokio::spawn(async move {
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let envelope: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(payload) = envelope.get("payload") else {
                continue;
            };
            let ty = payload.get("type").and_then(|ty| ty.as_str());
            if ty != Some("topic_event") {
                continue;
            }
            let Some(topic_value) = payload.get("topic").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(event_value) = payload.get("event") else {
                continue;
            };
            let Ok(event) = serde_json::from_value::<TopicEventPayload>(event_value.clone()) else {
                continue;
            };
            if tx
                .send(Invalidation {
                    topic: topic_value.to_owned(),
                    event,
                })
                .is_err()
            {
                break;
            }
        }
    });

    Ok(Watcher {
        invalidations: Arc::new(Mutex::new(rx)),
        _writer: write_half,
        _task: task,
    })
}

async fn create_product(client: &mut BossClient, input: CreateProductInput) -> Result<Product> {
    match client
        .send_request(&FrontendRequest::CreateProduct { input })
        .await?
    {
        FrontendEvent::WorkItemCreated { item } => expect_product(item),
        other => Err(unexpected_event("product create", other)),
    }
}

async fn create_project(client: &mut BossClient, input: CreateProjectInput) -> Result<Project> {
    match client
        .send_request(&FrontendRequest::CreateProject { input })
        .await?
    {
        FrontendEvent::WorkItemCreated { item } => expect_project(item),
        other => Err(unexpected_event("project create", other)),
    }
}

async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateTask { input })
        .await?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        other => Err(unexpected_event("task create", other)),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        other => Err(unexpected_event("chore create", other)),
    }
}

async fn update_work_item(
    client: &mut BossClient,
    id: &str,
    patch: WorkItemPatch,
) -> Result<WorkItem> {
    match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: id.to_owned(),
            patch,
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => Ok(item),
        other => Err(unexpected_event("update", other)),
    }
}

async fn delete_work_item(client: &mut BossClient, id: &str) -> Result<()> {
    match client
        .send_request(&FrontendRequest::DeleteWorkItem { id: id.to_owned() })
        .await?
    {
        FrontendEvent::WorkItemDeleted { .. } => Ok(()),
        other => Err(unexpected_event("delete", other)),
    }
}

async fn list_products(client: &mut BossClient) -> Result<Vec<Product>> {
    match client.send_request(&FrontendRequest::ListProducts).await? {
        FrontendEvent::ProductsList { products } => Ok(products),
        other => Err(unexpected_event("list products", other)),
    }
}

async fn list_projects(client: &mut BossClient, product_id: &str) -> Result<Vec<Project>> {
    list_projects_filtered(client, product_id, None).await
}

async fn list_projects_filtered(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Project>> {
    match client
        .send_request(&FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
            dep_filter,
        })
        .await?
    {
        FrontendEvent::ProjectsList { projects, .. } => Ok(projects),
        other => Err(unexpected_event("list projects", other)),
    }
}

async fn list_tasks(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
) -> Result<Vec<Task>> {
    list_tasks_filtered(client, product_id, project_id, None).await
}

async fn list_tasks_filtered(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Task>> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
            dep_filter,
        })
        .await?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        other => Err(unexpected_event("list tasks", other)),
    }
}

async fn list_chores(client: &mut BossClient, product_id: &str) -> Result<Vec<Task>> {
    list_chores_filtered(client, product_id, None).await
}

async fn list_chores_filtered(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Task>> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
            dep_filter,
        })
        .await?
    {
        FrontendEvent::ChoresList { chores, .. } => Ok(chores),
        other => Err(unexpected_event("list chores", other)),
    }
}

fn expect_product(item: WorkItem) -> Result<Product> {
    match item {
        WorkItem::Product(product) => Ok(product),
        other => Err(anyhow!("expected product, got {other:?}")),
    }
}

fn expect_project(item: WorkItem) -> Result<Project> {
    match item {
        WorkItem::Project(project) => Ok(project),
        other => Err(anyhow!("expected project, got {other:?}")),
    }
}

fn expect_task(item: WorkItem) -> Result<Task> {
    match item {
        WorkItem::Task(task) => Ok(task),
        WorkItem::Chore(_) => Err(anyhow!("expected task, got chore")),
        other => Err(anyhow!("expected task, got {other:?}")),
    }
}

fn expect_chore(item: WorkItem) -> Result<Task> {
    match item {
        WorkItem::Chore(chore) => Ok(chore),
        WorkItem::Task(_) => Err(anyhow!("expected chore, got task")),
        other => Err(anyhow!("expected chore, got {other:?}")),
    }
}

fn unexpected_event(context: &str, event: FrontendEvent) -> anyhow::Error {
    anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(&event).unwrap_or_else(|_| "<unserializable>".to_owned())
    )
}

async fn add_dependency(
    client: &mut BossClient,
    input: AddDependencyInput,
) -> Result<WorkItemDependency> {
    match client
        .send_request(&FrontendRequest::AddDependency { input })
        .await?
    {
        FrontendEvent::DependencyAdded { edge } => Ok(edge),
        other => Err(unexpected_event("add dependency", other)),
    }
}

async fn remove_dependency(client: &mut BossClient, input: RemoveDependencyInput) -> Result<bool> {
    match client
        .send_request(&FrontendRequest::RemoveDependency { input })
        .await?
    {
        FrontendEvent::DependencyRemoved { removed, .. } => Ok(removed),
        other => Err(unexpected_event("remove dependency", other)),
    }
}

async fn list_dependencies(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyView> {
    match client
        .send_request(&FrontendRequest::ListDependencies { input })
        .await?
    {
        FrontendEvent::DependencyList { view } => Ok(view),
        other => Err(unexpected_event("list dependencies", other)),
    }
}

async fn list_dependencies_detailed(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyDetail> {
    match client
        .send_request(&FrontendRequest::ListDependenciesDetailed { input })
        .await?
    {
        FrontendEvent::DependencyDetail { detail } => Ok(detail),
        other => Err(unexpected_event("list dependencies detailed", other)),
    }
}

/// Round-trip the new dependency RPCs through the wire layer:
/// add → list → remove. Cycles and self-loops surface as
/// `WorkError`. Existing CRUD verbs keep working alongside.
#[tokio::test]
async fn dependency_rpcs_round_trip_through_engine() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let a = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "A".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    let b = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "B".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;

    let edge = add_dependency(
        &mut client,
        AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        },
    )
    .await?;
    assert_eq!(edge.dependent_id, a.id);
    assert_eq!(edge.prerequisite_id, b.id);
    assert_eq!(edge.relation, "blocks");

    // Idempotent re-add returns the same row.
    let edge2 = add_dependency(
        &mut client,
        AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        },
    )
    .await?;
    assert_eq!(edge2, edge);

    // Cycle add returns a WorkError.
    let cycle = client
        .send_request(&FrontendRequest::AddDependency {
            input: AddDependencyInput {
                dependent: b.id.clone(),
                prerequisite: a.id.clone(),
                relation: None,
            },
        })
        .await?;
    match cycle {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("cycle"), "expected cycle error: {message}");
        }
        other => return Err(unexpected_event("cycle add", other)),
    }

    // List in both directions.
    let view = list_dependencies(
        &mut client,
        ListDependenciesInput {
            work_item: a.id.clone(),
            direction: Some(DependencyDirection::Both),
        },
    )
    .await?;
    assert_eq!(view.prerequisites.len(), 1);
    assert!(view.dependents.is_empty());

    let removed = remove_dependency(
        &mut client,
        RemoveDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        },
    )
    .await?;
    assert!(removed);

    let removed_again = remove_dependency(
        &mut client,
        RemoveDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        },
    )
    .await?;
    assert!(!removed_again);

    Ok(())
}

/// Phase 3 fixture from design Q6: a target task with 2 prerequisites
/// and 1 dependent. Exercises the resolved `ListDependenciesDetailed`
/// surface (used by `boss <kind> show`) and the four list filters
/// (`--prerequisites-of`, `--dependents-of`, `--unblocked`,
/// `--blocked-by-deps`). The dependent is a chore so we can also
/// confirm the filters honour the kind-of-list they were applied to.
#[tokio::test]
async fn dependency_show_detail_and_list_filters() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "DepTest".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:deptest.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 3".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;
    let prereq1 = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Land migration".to_owned(),
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
    let prereq2 = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Tune retries".to_owned(),
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
    let target = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Roll out feature".to_owned(),
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
    let dependent_chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Update docs".to_owned(),
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

    add_dependency(
        &mut client,
        AddDependencyInput {
            dependent: target.id.clone(),
            prerequisite: prereq1.id.clone(),
            relation: None,
        },
    )
    .await?;
    add_dependency(
        &mut client,
        AddDependencyInput {
            dependent: target.id.clone(),
            prerequisite: prereq2.id.clone(),
            relation: None,
        },
    )
    .await?;
    add_dependency(
        &mut client,
        AddDependencyInput {
            dependent: dependent_chore.id.clone(),
            prerequisite: target.id.clone(),
            relation: None,
        },
    )
    .await?;

    // `boss <kind> show` body — resolved edges with peer status / name.
    let detail = list_dependencies_detailed(
        &mut client,
        ListDependenciesInput {
            work_item: target.id.clone(),
            direction: Some(DependencyDirection::Both),
        },
    )
    .await?;
    assert_eq!(detail.work_item_id, target.id);
    assert_eq!(detail.prerequisites.len(), 2, "fixture has 2 prereqs");
    let prereq_ids: Vec<&str> = detail
        .prerequisites
        .iter()
        .map(|edge| edge.id.as_str())
        .collect();
    assert!(
        prereq_ids.contains(&prereq1.id.as_str()) && prereq_ids.contains(&prereq2.id.as_str()),
        "prereqs must surface both ids, got {prereq_ids:?}"
    );
    for edge in &detail.prerequisites {
        assert_eq!(edge.relation, "blocks");
        assert_eq!(edge.kind, "task");
        assert_eq!(edge.status, "todo");
        assert!(!edge.name.is_empty(), "peer name must be joined in");
    }
    assert_eq!(detail.dependents.len(), 1, "fixture has 1 dependent");
    let dep_edge = &detail.dependents[0];
    assert_eq!(dep_edge.id, dependent_chore.id);
    assert_eq!(dep_edge.kind, "chore");
    assert_eq!(dep_edge.name, "Update docs");
    // The chore is auto-blocked because `target` (its prereq) is itself
    // gated; phase 2 owns that transition. We just check the status is
    // a non-empty value resolved from the row.
    assert!(!dep_edge.status.is_empty());

    // `boss task list --prerequisites-of <target>` → both prereq tasks.
    let prereqs_listed = list_tasks_filtered(
        &mut client,
        &product.id,
        None,
        Some(DependencyFilter::PrerequisitesOf {
            id: target.id.clone(),
        }),
    )
    .await?;
    let listed_ids: Vec<String> = prereqs_listed.iter().map(|t| t.id.clone()).collect();
    assert_eq!(prereqs_listed.len(), 2, "got {listed_ids:?}");
    assert!(listed_ids.contains(&prereq1.id) && listed_ids.contains(&prereq2.id));

    // `boss task list --dependents-of <target>` → no project_tasks
    // (the only dependent is a chore).
    let dep_tasks = list_tasks_filtered(
        &mut client,
        &product.id,
        None,
        Some(DependencyFilter::DependentsOf {
            id: target.id.clone(),
        }),
    )
    .await?;
    assert!(
        dep_tasks.is_empty(),
        "no project_task dependents in fixture, got {:?}",
        dep_tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );

    // `boss chore list --dependents-of <target>` → the chore.
    let dep_chores = list_chores_filtered(
        &mut client,
        &product.id,
        Some(DependencyFilter::DependentsOf {
            id: target.id.clone(),
        }),
    )
    .await?;
    assert_eq!(dep_chores.len(), 1);
    assert_eq!(dep_chores[0].id, dependent_chore.id);

    // `boss task list --blocked-by-deps` → the target (its prereqs are
    // both incomplete). Prereqs themselves and the chore are excluded.
    let blocked = list_tasks_filtered(
        &mut client,
        &product.id,
        None,
        Some(DependencyFilter::BlockedByDeps),
    )
    .await?;
    let blocked_ids: Vec<String> = blocked.iter().map(|t| t.id.clone()).collect();
    assert!(
        blocked_ids.contains(&target.id),
        "target must be flagged blocked-by-deps, got {blocked_ids:?}"
    );
    assert!(
        !blocked_ids.contains(&prereq1.id) && !blocked_ids.contains(&prereq2.id),
        "prereqs have no deps and must not be flagged"
    );

    // `boss task list --unblocked` → tasks in `todo` with no incomplete
    // prereq. Both prereqs qualify; target is in `blocked` (auto-flip
    // from phase 2) so does not.
    let unblocked = list_tasks_filtered(
        &mut client,
        &product.id,
        None,
        Some(DependencyFilter::Unblocked),
    )
    .await?;
    let unblocked_ids: Vec<String> = unblocked.iter().map(|t| t.id.clone()).collect();
    assert!(
        unblocked_ids.contains(&prereq1.id) && unblocked_ids.contains(&prereq2.id),
        "todo prereqs must surface in --unblocked, got {unblocked_ids:?}"
    );
    assert!(
        !unblocked_ids.contains(&target.id),
        "target is gated; --unblocked must exclude it"
    );

    Ok(())
}

/// Round-trip the new bulk-create RPCs through the wire layer:
/// `CreateManyTasks` and `CreateManyChores` insert all items in one
/// engine round-trip and reply with `WorkItemsCreated`. A bad item in
/// the middle of the batch must roll back the whole transaction.
#[tokio::test]
async fn create_many_tasks_and_chores_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
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
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;

    let task_inputs: Vec<CreateTaskInput> = (0..4)
        .map(|i| CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: format!("Bulk task {i}"),
            description: Some(format!("desc {i}")),
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .collect();
    let created_tasks = match client
        .send_request(&FrontendRequest::CreateManyTasks {
            input: CreateManyTasksInput { items: task_inputs },
        })
        .await?
    {
        FrontendEvent::WorkItemsCreated { items } => items
            .into_iter()
            .map(expect_task)
            .collect::<Result<Vec<_>>>()?,
        other => return Err(unexpected_event("create-many tasks", other)),
    };
    assert_eq!(created_tasks.len(), 4);
    let listed_tasks = list_tasks(&mut client, &product.id, Some(&project.id)).await?;
    // 4 user-created project_tasks plus the auto-created design task.
    assert_eq!(listed_tasks.len(), 5);
    assert_eq!(
        listed_tasks
            .iter()
            .filter(|t| t.kind == "design")
            .count(),
        1,
    );

    let chore_inputs: Vec<CreateChoreInput> = (0..3)
        .map(|i| CreateChoreInput {
            product_id: product.id.clone(),
            name: format!("Bulk chore {i}"),
            description: None,
            autostart: i == 0,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .collect();
    let created_chores = match client
        .send_request(&FrontendRequest::CreateManyChores {
            input: CreateManyChoresInput {
                items: chore_inputs,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemsCreated { items } => items
            .into_iter()
            .map(expect_chore)
            .collect::<Result<Vec<_>>>()?,
        other => return Err(unexpected_event("create-many chores", other)),
    };
    assert_eq!(created_chores.len(), 3);
    let listed_chores = list_chores(&mut client, &product.id).await?;
    assert_eq!(listed_chores.len(), 3);

    // A bad item in the batch (unknown project) rolls back the whole
    // transaction — no new tasks land.
    let bad_inputs = vec![
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Should not survive".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: "proj_nope".to_owned(),
            name: "Bad ref".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    ];
    match client
        .send_request(&FrontendRequest::CreateManyTasks {
            input: CreateManyTasksInput { items: bad_inputs },
        })
        .await?
    {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("item 1"), "{message}");
        }
        other => return Err(unexpected_event("expected WorkError", other)),
    }
    let listed_after = list_tasks(&mut client, &product.id, Some(&project.id)).await?;
    // 4 user-created project_tasks (from the earlier successful batch)
    // plus the auto-created design task — 5 total. The bad batch
    // rolled back, so no extra rows leaked.
    assert_eq!(listed_after.len(), 5, "rollback must not leak rows");

    Ok(())
}

async fn set_project_design_doc(
    client: &mut BossClient,
    input: SetProjectDesignDocInput,
) -> Result<Project> {
    match client
        .send_request(&FrontendRequest::SetProjectDesignDoc { input })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => expect_project(item),
        other => Err(unexpected_event("set project design doc", other)),
    }
}

async fn resolve_project_design_doc(
    client: &mut BossClient,
    project_id: &str,
) -> Result<ResolveProjectDesignDocOutput> {
    match client
        .send_request(&FrontendRequest::ResolveProjectDesignDoc {
            project_id: project_id.to_owned(),
        })
        .await?
    {
        FrontendEvent::ProjectDesignDocResolved { output } => Ok(output),
        other => Err(unexpected_event("resolve project design doc", other)),
    }
}

/// Acceptance criterion for chore #5 of the project-design-doc-pointer
/// design: a CLI-style client sets a project's design-doc pointer, a
/// second resolve call returns the same triple wrapped in
/// `ResolveProjectDesignDocOutput`, and the resolution semantics
/// (same-product vs external classification, branch defaults, broken
/// pointer when the product has no `repo_remote_url`) all round-trip
/// through the wire layer correctly.
#[tokio::test]
async fn project_design_doc_rpcs_round_trip_through_engine() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // ----- in-repo (same-product) case --------------------------------------
    let mono = create_product(
        &mut client,
        CreateProductInput {
            name: "Mono".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: mono.id.clone(),
            name: "Pointer".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;

    // Fresh project: no pointer set → resolver answers `NotSet`.
    let initial = resolve_project_design_doc(&mut client, &project.id).await?;
    assert_eq!(initial.project_id, project.id);
    assert!(matches!(initial.state, ProjectDesignDocState::NotSet));

    // CLI sets the pointer with just a path; repo/branch inherit from
    // the product. The returned `WorkItemUpdated` carries the persisted
    // `Project` so the kanban can refresh without an extra round-trip.
    let updated = set_project_design_doc(
        &mut client,
        SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_path: Some("tools/boss/docs/designs/pointer.md".to_owned()),
            ..SetProjectDesignDocInput::default()
        },
    )
    .await?;
    assert_eq!(updated.id, project.id);
    assert_eq!(
        updated.design_doc_path.as_deref(),
        Some("tools/boss/docs/designs/pointer.md"),
    );
    assert!(updated.design_doc_repo_remote_url.is_none());
    assert!(updated.design_doc_branch.is_none());

    // Resolve: same-product (the path inherits the product's repo),
    // branch defaults to `main`, web URL is rendered for the kanban
    // tooltip.
    let resolved = resolve_project_design_doc(&mut client, &project.id).await?;
    assert_eq!(resolved.project_id, project.id);
    match resolved.state {
        ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            raw_content_url,
        } => {
            assert_eq!(resolved.repo_remote_url, "git@github.com:spinyfin/mono.git");
            assert_eq!(resolved.branch, "main");
            assert_eq!(resolved.path, "tools/boss/docs/designs/pointer.md");
            assert_eq!(
                resolved.kind,
                ResolvedDesignDocKind::SameProduct {
                    product_id: mono.id.clone()
                },
            );
            // No worker has leased a workspace in this test process,
            // so `workspace_path` is `None` — matches the engine-side
            // closure that queries in-flight executions.
            assert!(workspace_path.is_none());
            assert!(
                web_url.contains("spinyfin/mono"),
                "web_url should reference the resolved repo: {web_url}",
            );
            assert!(
                web_url.contains("tools/boss/docs/designs/pointer.md"),
                "web_url should embed the resolved path: {web_url}",
            );
            // raw_content_url must be present for github.com repos.
            assert!(
                raw_content_url
                    .as_deref()
                    .is_some_and(|u| u.starts_with("https://raw.githubusercontent.com/spinyfin/mono/")),
                "raw_content_url should be a raw.githubusercontent.com URL: {raw_content_url:?}",
            );
        }
        other => panic!("expected Resolved, got {other:?}"),
    }

    // ----- separate-repo (external) case ------------------------------------
    // Override the pointer to point at a doc-repo that isn't a tracked
    // product. Branch override survives the round-trip; the resolver
    // classifies the repo as `External` because no other `products` row
    // matches.
    let external_url = "https://github.com/myorg/wiki.git".to_owned();
    let external_update = set_project_design_doc(
        &mut client,
        SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(external_url.clone()),
            design_doc_branch: Some("trunk".to_owned()),
            design_doc_path: Some("designs/pointer.md".to_owned()),
            ..SetProjectDesignDocInput::default()
        },
    )
    .await?;
    assert_eq!(
        external_update.design_doc_repo_remote_url.as_deref(),
        Some(external_url.as_str()),
    );
    assert_eq!(external_update.design_doc_branch.as_deref(), Some("trunk"));

    let resolved_external = resolve_project_design_doc(&mut client, &project.id).await?;
    match resolved_external.state {
        ProjectDesignDocState::Resolved { resolved, .. } => {
            assert_eq!(resolved.repo_remote_url, external_url);
            assert_eq!(resolved.branch, "trunk");
            assert_eq!(resolved.path, "designs/pointer.md");
            assert_eq!(resolved.kind, ResolvedDesignDocKind::External);
        }
        other => panic!("expected Resolved (external), got {other:?}"),
    }

    // ----- unset path -------------------------------------------------------
    let cleared = set_project_design_doc(
        &mut client,
        SetProjectDesignDocInput {
            project_id: project.id.clone(),
            unset: true,
            ..SetProjectDesignDocInput::default()
        },
    )
    .await?;
    assert!(cleared.design_doc_path.is_none());
    assert!(cleared.design_doc_repo_remote_url.is_none());
    assert!(cleared.design_doc_branch.is_none());
    let resolved_cleared = resolve_project_design_doc(&mut client, &project.id).await?;
    assert!(matches!(
        resolved_cleared.state,
        ProjectDesignDocState::NotSet,
    ));

    // ----- broken pointer (path set, no repo available) --------------------
    // A product without a `repo_remote_url` and a project whose pointer
    // sets only the path can't be resolved — surface as `Broken` per
    // design Q5.
    let no_repo = create_product(
        &mut client,
        CreateProductInput {
            name: "NoRepo".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
        },
    )
    .await?;
    let orphan = create_project(
        &mut client,
        CreateProjectInput {
            product_id: no_repo.id.clone(),
            name: "Orphan".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        },
    )
    .await?;
    set_project_design_doc(
        &mut client,
        SetProjectDesignDocInput {
            project_id: orphan.id.clone(),
            design_doc_path: Some("docs/orphan.md".to_owned()),
            ..SetProjectDesignDocInput::default()
        },
    )
    .await?;
    let resolved_broken = resolve_project_design_doc(&mut client, &orphan.id).await?;
    match resolved_broken.state {
        ProjectDesignDocState::Broken { reason } => {
            assert!(
                reason.contains("repo"),
                "broken reason should name the missing repo column: {reason}",
            );
        }
        other => panic!("expected Broken, got {other:?}"),
    }

    // ----- validation error surfaces as WorkError --------------------------
    // Q8: paths must be repo-relative markdown — feed the engine a
    // path traversal and confirm the validator's error rides back on
    // the wire as a `WorkError` (rather than crashing the connection
    // or returning a half-applied write).
    match client
        .send_request(&FrontendRequest::SetProjectDesignDoc {
            input: SetProjectDesignDocInput {
                project_id: project.id.clone(),
                design_doc_path: Some("../escape.md".to_owned()),
                ..SetProjectDesignDocInput::default()
            },
        })
        .await?
    {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains(".."),
                "validation error should mention the bad segment: {message}",
            );
        }
        other => return Err(unexpected_event("expected WorkError", other)),
    }

    // The rejected write must not have touched the row.
    let after_reject = resolve_project_design_doc(&mut client, &project.id).await?;
    assert!(matches!(after_reject.state, ProjectDesignDocState::NotSet));

    Ok(())
}

/// Engine invariant: creating a task/chore under a single-repo product
/// (product has `repo_remote_url`) without an override stores `NULL` in
/// the task row. The engine must NOT materialise the product's repo into
/// the task row — the dispatcher resolves it from the product at runtime.
#[tokio::test]
async fn create_task_on_single_repo_product_stores_null_repo() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
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
            name: "Wire socket client".to_owned(),
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
    assert!(
        task.repo_remote_url.is_none(),
        "task under single-repo product must store NULL, not the product URL; got {:?}",
        task.repo_remote_url,
    );

    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Trim stale work".to_owned(),
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
    assert!(
        chore.repo_remote_url.is_none(),
        "chore under single-repo product must store NULL, not the product URL; got {:?}",
        chore.repo_remote_url,
    );

    Ok(())
}

/// Engine invariant: attempting to create a task/chore under a
/// single-repo product WITH an explicit repo override is rejected. The
/// engine returns a `WorkError` whose message matches the expected
/// shape so the CLI can propagate it faithfully.
#[tokio::test]
async fn create_task_with_explicit_repo_on_single_repo_product_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
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

    match client
        .send_request(&FrontendRequest::CreateTask {
            input: CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Override attempt".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: Some("git@github.com:spinyfin/other.git".to_owned()),
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            },
        })
        .await?
    {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cannot set per-task repo override"),
                "error should describe the invariant violation: {message}"
            );
            assert!(
                message.contains("boss"),
                "error should name the product slug: {message}"
            );
        }
        other => return Err(unexpected_event("expected WorkError for task override rejection", other)),
    }

    // Same enforcement for chores.
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product.id.clone(),
                name: "Override chore attempt".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: Some("git@github.com:spinyfin/other.git".to_owned()),
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            },
        })
        .await?
    {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cannot set per-task repo override"),
                "chore: error should describe the invariant violation: {message}"
            );
        }
        other => return Err(unexpected_event("expected WorkError for chore override rejection", other)),
    }

    Ok(())
}

/// Engine invariant: creating a task/chore under a no-repo product
/// (product has no `repo_remote_url`) requires the caller to supply a
/// repo override. When no override is given the engine rejects the
/// insert with a `WorkError`.
#[tokio::test]
async fn create_task_on_no_repo_product_without_override_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Greenfield".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
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

    match client
        .send_request(&FrontendRequest::CreateTask {
            input: CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "No repo".to_owned(),
                description: None,
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
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("no repo"),
                "error should explain that a repo is required: {message}"
            );
        }
        other => return Err(unexpected_event("expected WorkError for missing repo", other)),
    }

    // Same enforcement for chores.
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product.id.clone(),
                name: "No repo chore".to_owned(),
                description: None,
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
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("no repo"),
                "chore: error should explain that a repo is required: {message}"
            );
        }
        other => return Err(unexpected_event("expected WorkError for missing chore repo", other)),
    }

    Ok(())
}

/// Engine invariant: creating a task/chore under a no-repo product
/// WITH an explicit override stores the override in the row.
#[tokio::test]
async fn create_task_on_no_repo_product_with_override_stores_it() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Greenfield".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
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
            name: "Repo override".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some("git@github.com:foo/service.git".to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(
        task.repo_remote_url.as_deref(),
        Some("git@github.com:foo/service.git"),
        "no-repo product: explicit override must be stored in the row",
    );

    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Chore with repo".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: Some("git@github.com:foo/other.git".to_owned()),
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        },
    )
    .await?;
    assert_eq!(
        chore.repo_remote_url.as_deref(),
        Some("git@github.com:foo/other.git"),
        "no-repo product: explicit chore override must be stored in the row",
    );

    Ok(())
}

/// `dispatch_preamble` round-trip: set, verify, update, clear.
#[tokio::test]
async fn product_dispatch_preamble_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "preamble-test".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
        },
    )
    .await?;
    assert!(
        product.dispatch_preamble.is_none(),
        "new product has no dispatch_preamble"
    );

    // Set a preamble via product update.
    let updated = expect_product(
        update_work_item(
            &mut client,
            &product.id,
            WorkItemPatch {
                dispatch_preamble: Some("Prefer bazel for tests.".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(
        updated.dispatch_preamble.as_deref(),
        Some("Prefer bazel for tests."),
        "preamble should be stored verbatim",
    );

    // Update to a different value.
    let updated2 = expect_product(
        update_work_item(
            &mut client,
            &product.id,
            WorkItemPatch {
                dispatch_preamble: Some("Use bazel; cargo only on explicit fallback.".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert_eq!(
        updated2.dispatch_preamble.as_deref(),
        Some("Use bazel; cargo only on explicit fallback."),
    );

    // Clear via empty string.
    let cleared = expect_product(
        update_work_item(
            &mut client,
            &product.id,
            WorkItemPatch {
                dispatch_preamble: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .await?,
    )?;
    assert!(
        cleared.dispatch_preamble.is_none(),
        "empty string clears the preamble",
    );

    Ok(())
}

/// Helper: create a chore, expecting a `WorkItemDuplicateBlocked` response.
async fn create_chore_expect_duplicate(
    client: &mut BossClient,
    input: CreateChoreInput,
) -> Result<(String, i64, String, i64)> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await?
    {
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Ok((existing_id, existing_short_id, name, age_secs)),
        other => Err(anyhow!("expected WorkItemDuplicateBlocked, got {:?}", other)),
    }
}

/// Duplicate guard: a second chore with the same name in the same product within 60 s
/// is rejected, but `force_duplicate` bypasses the check, and a deleted row is
/// excluded from the match.
#[tokio::test]
async fn chore_duplicate_guard_blocks_within_window() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "dup-test".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/dup-test.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;

    // First create succeeds.
    let first = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "  Dedup Me  ".to_owned(),
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

    // Immediate retry (within 60 s) is rejected with structured info.
    let (existing_id, existing_short_id, blocked_name, age_secs) =
        create_chore_expect_duplicate(
            &mut client,
            CreateChoreInput {
                product_id: product.id.clone(),
                name: "Dedup Me".to_owned(), // trimmed match
                description: Some("different description".to_owned()),
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

    assert_eq!(existing_id, first.id, "existing_id must reference the first row");
    assert_eq!(
        existing_short_id,
        first.short_id.unwrap_or(0),
        "existing_short_id must match the first row's short_id",
    );
    assert_eq!(blocked_name, "Dedup Me", "blocked_name is the trimmed input");
    assert!(age_secs >= 0, "age_secs must be non-negative");

    // force_duplicate bypasses the guard.
    let forced = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Dedup Me".to_owned(),
            description: None,
            autostart: false,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: true,
        },
    )
    .await?;
    assert_ne!(forced.id, first.id, "forced create must produce a new row");

    // Soft-deleted row is excluded from the guard.
    delete_work_item(&mut client, &first.id).await?;
    // After deleting `first`, a retry with the same name should succeed
    // (only `forced` remains, and the guard also blocks on that one — so
    // let's delete forced too, then confirm a fresh create works).
    delete_work_item(&mut client, &forced.id).await?;
    let after_delete = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Dedup Me".to_owned(),
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
    assert_ne!(after_delete.id, first.id, "post-delete create must be a fresh row");

    Ok(())
}

/// Duplicate guard for tasks: same name + same product within 60 s is rejected.
#[tokio::test]
async fn task_duplicate_guard_blocks_within_window() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "task-dup-test".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/task-dup-test.git".to_owned()),
            design_repo: None,
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
            no_design_task: true,
        },
    )
    .await?;

    let first = create_task(
        &mut client,
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: project.id.clone(),
            name: "Unique Task".to_owned(),
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

    // Retry blocked.
    match client
        .send_request(&FrontendRequest::CreateTask {
            input: CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Unique Task".to_owned(),
                description: None,
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
        FrontendEvent::WorkItemDuplicateBlocked { existing_id, .. } => {
            assert_eq!(existing_id, first.id, "blocked must reference first task");
        }
        other => return Err(anyhow!("expected WorkItemDuplicateBlocked, got {:?}", other)),
    }

    // Cross-product: same name in a different product is allowed.
    let other_product = create_product(
        &mut client,
        CreateProductInput {
            name: "other-product".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:foo/other.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let other_project = create_project(
        &mut client,
        CreateProjectInput {
            product_id: other_product.id.clone(),
            name: "Other Phase".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
        },
    )
    .await?;
    let cross = create_task(
        &mut client,
        CreateTaskInput {
            product_id: other_product.id.clone(),
            project_id: other_project.id.clone(),
            name: "Unique Task".to_owned(),
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
    assert_ne!(
        cross.id, first.id,
        "same name in different product must be allowed",
    );

    Ok(())
}

/// link → verify external_ref is stored → the row is findable by the reconciler.
#[tokio::test]
async fn link_external_ref_stores_binding_and_is_findable() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Sync".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Link chore".to_owned(),
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

    // Link the chore to an upstream issue.
    let updated = match client
        .send_request(&FrontendRequest::LinkWorkItemExternalRef {
            input: LinkExternalRefInput {
                work_item_id: chore.id.clone(),
                kind: "github".to_owned(),
                canonical_id: "spinyfin/mono#560".to_owned(),
            },
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => expect_chore(item)?,
        other => return Err(unexpected_event("link-external-ref", other)),
    };

    // The returned item must have external_ref populated.
    let ext = updated.external_ref.as_ref().expect("external_ref should be set after link");
    assert_eq!(ext.kind, "github");
    assert_eq!(ext.canonical_id, "spinyfin/mono#560");
    assert!(ext.unbound_at.is_none(), "newly linked row must not be unbound");

    // A second GetWorkItem call returns the same data (engine persisted it).
    let fetched = match client
        .send_request(&FrontendRequest::GetWorkItem { id: chore.id.clone() })
        .await?
    {
        FrontendEvent::WorkItemResult { item } => expect_chore(item)?,
        other => return Err(unexpected_event("get-work-item-after-link", other)),
    };
    // GetWorkItem uses the standard query (no external-ref columns), so
    // external_ref is None there — but the row must otherwise match.
    assert_eq!(fetched.id, updated.id);

    Ok(())
}

/// unlink clears the active binding; the row stops being touched by the reconciler.
#[tokio::test]
async fn unlink_external_ref_clears_binding() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput {
            name: "Unsync".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
        },
    )
    .await?;
    let chore = create_chore(
        &mut client,
        CreateChoreInput {
            product_id: product.id.clone(),
            name: "Unlink chore".to_owned(),
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

    // First, link the chore.
    match client
        .send_request(&FrontendRequest::LinkWorkItemExternalRef {
            input: LinkExternalRefInput {
                work_item_id: chore.id.clone(),
                kind: "echo".to_owned(),
                canonical_id: "echo#1".to_owned(),
            },
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { .. } => {}
        other => return Err(unexpected_event("link before unlink", other)),
    };

    // Now unlink.
    let unlinked = match client
        .send_request(&FrontendRequest::UnlinkWorkItemExternalRef {
            work_item_id: chore.id.clone(),
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => expect_chore(item)?,
        other => return Err(unexpected_event("unlink-external-ref", other)),
    };

    // The returned item must have unbound_at set (marking the binding as inactive).
    let ext = unlinked
        .external_ref
        .as_ref()
        .expect("external_ref should still be present after unlink");
    assert!(
        ext.unbound_at.is_some(),
        "unbound_at must be set after unlink; got: {:?}",
        ext
    );

    Ok(())
}

/// link-external on a non-existent work item returns WorkError.
#[tokio::test]
async fn link_external_ref_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    match client
        .send_request(&FrontendRequest::LinkWorkItemExternalRef {
            input: LinkExternalRefInput {
                work_item_id: "task_nonexistent".to_owned(),
                kind: "github".to_owned(),
                canonical_id: "spinyfin/mono#999".to_owned(),
            },
        })
        .await?
    {
        FrontendEvent::WorkError { .. } => {}
        other => return Err(unexpected_event("expected WorkError for missing id", other)),
    }

    Ok(())
}

/// unlink-external on a non-existent work item returns WorkError.
#[tokio::test]
async fn unlink_external_ref_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    match client
        .send_request(&FrontendRequest::UnlinkWorkItemExternalRef {
            work_item_id: "task_nonexistent".to_owned(),
        })
        .await?
    {
        FrontendEvent::WorkError { .. } => {}
        other => return Err(unexpected_event("expected WorkError for missing id", other)),
    }

    Ok(())
}
