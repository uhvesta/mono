//! Shared test-support helper for the socket-event subscription harness used
//! by the `work_crud` and `comments_crud` integration tests. Each integration
//! test is its own `rust_test` target, so this file is listed in the `srcs` of
//! both targets and pulled in via `mod watcher_support;` from each test file.
//!
//! `subscribe_watcher` opens a second client connection, subscribes to a topic,
//! drains the Hello + Subscribed acknowledgements, then spawns a background task
//! that decodes `topic_event` payloads into `Invalidation`s on an mpsc channel.

// Not every consumer touches every item, and each integration test binary
// compiles this file independently; suppress dead-code noise rather than gate
// individual items per crate.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_protocol::TopicEventPayload;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::Mutex;

pub struct Invalidation {
    pub topic: String,
    pub event: TopicEventPayload,
}

pub struct Watcher {
    invalidations: Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<Invalidation>>>,
    _writer: OwnedWriteHalf,
    _task: tokio::task::JoinHandle<()>,
}

impl Watcher {
    pub async fn next_invalidation(&self, timeout: Duration) -> Result<Invalidation> {
        let mut rx = self.invalidations.lock().await;
        tokio::time::timeout(timeout, rx.recv())
            .await
            .map_err(|_| anyhow!("timed out waiting for invalidation"))?
            .ok_or_else(|| anyhow!("watcher channel closed"))
    }
}

pub async fn subscribe_watcher(socket_path: &str, topic: String) -> Result<Watcher> {
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
