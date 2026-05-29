//! Integration test: drive the Phase-2 `comments_*` RPCs through the wire
//! layer against an in-process engine, and confirm the
//! `comments.artifact.*` subscription topic fires on a mutation. Mirrors
//! the harness in `work_crud.rs`. Design:
//! `tools/boss/docs/designs/comments-in-markdown-viewer.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CommentAnchor, CreateCommentInput, FrontendEvent, FrontendRequest, ResolvedComment,
    TopicEventPayload, WorkComment, comment_topic,
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
            db_path: PathBuf::from(":memory:"),
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));
        let socket_for_serve = socket_path.clone();
        let join =
            tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });
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

fn anchor(exact: &str, prefix: &str, suffix: &str) -> CommentAnchor {
    CommentAnchor {
        exact: exact.to_owned(),
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
    }
}

async fn create_comment(client: &mut BossClient, input: CreateCommentInput) -> Result<WorkComment> {
    match client
        .send_request(&FrontendRequest::CommentsCreate { input })
        .await?
    {
        FrontendEvent::CommentResult { comment } => Ok(comment),
        other => Err(unexpected("comments_create", other)),
    }
}

async fn list_comments(
    client: &mut BossClient,
    artifact_kind: &str,
    artifact_id: &str,
    include_resolved: bool,
) -> Result<Vec<WorkComment>> {
    match client
        .send_request(&FrontendRequest::CommentsList {
            artifact_kind: artifact_kind.to_owned(),
            artifact_id: artifact_id.to_owned(),
            include_resolved,
        })
        .await?
    {
        FrontendEvent::CommentsList { comments, .. } => Ok(comments),
        other => Err(unexpected("comments_list", other)),
    }
}

async fn resolve_comments(
    client: &mut BossClient,
    artifact_kind: &str,
    artifact_id: &str,
    plain_text: &str,
) -> Result<Vec<ResolvedComment>> {
    match client
        .send_request(&FrontendRequest::CommentsResolve {
            artifact_kind: artifact_kind.to_owned(),
            artifact_id: artifact_id.to_owned(),
            plain_text: plain_text.to_owned(),
            plain_text_projection_version: 1,
        })
        .await?
    {
        FrontendEvent::CommentsResolved { comments, .. } => Ok(comments),
        other => Err(unexpected("comments_resolve", other)),
    }
}

async fn dismiss_comment(client: &mut BossClient, comment_id: &str) -> Result<WorkComment> {
    match client
        .send_request(&FrontendRequest::CommentsDismiss {
            comment_id: comment_id.to_owned(),
            actor: Some("user:me".to_owned()),
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => Ok(comment),
        other => Err(unexpected("comments_dismiss", other)),
    }
}

fn unexpected(context: &str, event: FrontendEvent) -> anyhow::Error {
    anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(&event).unwrap_or_else(|_| "<unserializable>".to_owned())
    )
}

#[tokio::test]
async fn comments_create_list_resolve_dismiss_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let kind = "work_item";
    let id = "task_round_trip";
    let doc = "Alpha beta gamma. The sample span sits here. Delta epsilon.";

    let c1 = create_comment(
        &mut client,
        CreateCommentInput {
            artifact_kind: kind.to_owned(),
            artifact_id: id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: anchor("sample span", "The ", " sits here"),
            body: "first comment".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        },
    )
    .await?;
    assert_eq!(c1.status, "active");
    assert_eq!(c1.anchor.exact, "sample span");

    let _c2 = create_comment(
        &mut client,
        CreateCommentInput {
            artifact_kind: kind.to_owned(),
            artifact_id: id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: anchor("Delta epsilon", "here. ", ""),
            body: "second comment".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        },
    )
    .await?;

    // List returns both active comments.
    assert_eq!(list_comments(&mut client, kind, id, false).await?.len(), 2);

    // Resolve against the current doc: the first anchor exact-matches.
    let resolved = resolve_comments(&mut client, kind, id, doc).await?;
    assert_eq!(resolved.len(), 2);
    let first = resolved
        .iter()
        .find(|r| r.comment.id == c1.id)
        .expect("c1 present in resolution");
    assert_eq!(first.resolution.kind, "exact");
    let start = first.resolution.start.unwrap() as usize;
    let length = first.resolution.length.unwrap() as usize;
    let span: String = doc.chars().skip(start).take(length).collect();
    assert_eq!(span, "sample span");

    // Resolve against a doc where the first span is gone → orphan.
    let edited = "Alpha beta gamma. Delta epsilon. Nothing else remains in this body.";
    let reresolved = resolve_comments(&mut client, kind, id, edited).await?;
    let first_again = reresolved
        .iter()
        .find(|r| r.comment.id == c1.id)
        .expect("c1 present");
    assert_eq!(first_again.resolution.kind, "orphan");

    // Soft-dismiss the first comment: hidden by default, revealed with the
    // include_resolved toggle.
    let dismissed = dismiss_comment(&mut client, &c1.id).await?;
    assert_eq!(dismissed.status, "resolved");
    let default_list = list_comments(&mut client, kind, id, false).await?;
    assert!(default_list.iter().all(|c| c.id != c1.id));
    let full_list = list_comments(&mut client, kind, id, true).await?;
    assert!(full_list.iter().any(|c| c.id == c1.id && c.status == "resolved"));

    Ok(())
}

#[tokio::test]
async fn comment_topic_invalidation_reaches_subscriber() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut writer = BossClient::connect_socket(engine.socket_str()).await?;

    let kind = "work_item";
    let id = "task_sub";
    let topic = comment_topic(kind, id);
    let watcher = subscribe_watcher(engine.socket_str(), topic.clone()).await?;

    let _c = create_comment(
        &mut writer,
        CreateCommentInput {
            artifact_kind: kind.to_owned(),
            artifact_id: id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: anchor("anything", "", ""),
            body: "watch me".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        },
    )
    .await?;

    let invalidation = watcher.next_invalidation(Duration::from_secs(2)).await?;
    assert_eq!(invalidation.topic, topic);
    match invalidation.event {
        TopicEventPayload::WorkInvalidated {
            reason, item_ids, ..
        } => {
            assert_eq!(reason, "comment_created");
            assert_eq!(item_ids, vec![id.to_owned()]);
        }
        TopicEventPayload::ExecutionInvalidated { .. } => {
            panic!("unexpected execution invalidation on comment topic")
        }
    }

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

    let envelope = serde_json::json!({
        "request_id": "watcher-subscribe-1",
        "payload": { "type": "subscribe", "topics": [topic.clone()] }
    });
    let line = serde_json::to_string(&envelope)?;
    write_half.write_all(line.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

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
            if payload.get("type").and_then(|ty| ty.as_str()) != Some("topic_event") {
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
