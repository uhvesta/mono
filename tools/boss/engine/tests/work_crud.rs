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
    CreateProductInput, CreateProjectInput, CreateTaskInput, DependencyDirection, FrontendEvent,
    FrontendRequest, ListDependenciesInput, Product, Project, RemoveDependencyInput, Task,
    TopicEventPayload, WorkItem, WorkItemDependency, WorkItemDependencyView, WorkItemPatch,
    work_product_topic,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::Mutex;

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
    assert_eq!(listed_tasks.len(), 1);
    assert_eq!(listed_tasks[0].id, task.id);

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
async fn second_client_receives_invalidation_from_first() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    let mut writer_client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(
        &mut writer_client,
        CreateProductInput {
            name: "Multiplex".to_owned(),
            description: None,
            repo_remote_url: None,
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
            repo_remote_url: None,
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
            repo_remote_url: None,
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
            repo_remote_url: None,
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
    match client
        .send_request(&FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
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
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
        })
        .await?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        other => Err(unexpected_event("list tasks", other)),
    }
}

async fn list_chores(client: &mut BossClient, product_id: &str) -> Result<Vec<Task>> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
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
    assert_eq!(listed_tasks.len(), 4);

    let chore_inputs: Vec<CreateChoreInput> = (0..3)
        .map(|i| CreateChoreInput {
            product_id: product.id.clone(),
            name: format!("Bulk chore {i}"),
            description: None,
            autostart: i == 0,
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
        },
        CreateTaskInput {
            product_id: product.id.clone(),
            project_id: "proj_nope".to_owned(),
            name: "Bad ref".to_owned(),
            description: None,
            autostart: false,
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
    assert_eq!(listed_after.len(), 4, "rollback must not leak rows");

    Ok(())
}
