//! Incremental JSONL tail-watcher for claude transcript files.
//!
//! Each claude session writes a transcript JSONL file at a path the
//! engine records on `WorkRun.transcript_path`. The hooks-to-socket
//! channel gives us discrete events; the transcript carries richer
//! per-token content. This module is the primitive that streams that
//! file as it grows, returning each newly-written JSONL line as a
//! parsed [`serde_json::Value`].
//!
//! The watcher tolerates:
//!
//! - The file not existing yet (returns no events; will pick up content
//!   when it appears).
//! - Partial trailing lines (held in an internal buffer and flushed on
//!   the next poll once the line completes).
//! - Truncation / rotation (size shrinking below our cursor resets the
//!   cursor and re-reads from the start).
//!
//! Callers choose the polling cadence themselves — this module exposes
//! a single [`TranscriptTail::poll`] method rather than spawning a task.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

#[derive(Debug, Error)]
pub enum TailError {
    #[error("transcript io: {0}")]
    Io(#[from] io::Error),
    #[error("transcript bytes are not valid utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("transcript line is not valid JSON: {error} (line: {line:?})")]
    Json {
        error: serde_json::Error,
        line: String,
    },
}

#[derive(Debug)]
pub struct TranscriptTail {
    path: PathBuf,
    offset: u64,
    /// Buffer for a trailing partial line carried across polls.
    partial: String,
}

impl TranscriptTail {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            partial: String::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read any new content since the last poll and return parsed JSONL
    /// values. Returns an empty vec if the file does not exist or has
    /// not grown.
    pub async fn poll(&mut self) -> Result<Vec<serde_json::Value>, TailError> {
        let mut file = match fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let metadata = file.metadata().await?;
        let size = metadata.len();

        // Truncation / rotation: file shrunk below our cursor, so the
        // log was rolled. Reset and re-read from the top.
        if size < self.offset {
            self.offset = 0;
            self.partial.clear();
        }

        if size == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset)).await?;
        let to_read = (size - self.offset) as usize;
        let mut buf = Vec::with_capacity(to_read);
        file.read_to_end(&mut buf).await?;
        self.offset = size;

        let chunk = String::from_utf8(buf)?;
        let mut combined = std::mem::take(&mut self.partial);
        combined.push_str(&chunk);

        let mut values = Vec::new();
        let mut start = 0usize;
        let bytes = combined.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                let line = &combined[start..i];
                start = i + 1;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => values.push(value),
                    Err(error) => {
                        return Err(TailError::Json {
                            error,
                            line: trimmed.to_owned(),
                        });
                    }
                }
            }
        }
        self.partial = combined[start..].to_owned();

        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    fn sample_path(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[tokio::test]
    async fn missing_file_yields_no_events() {
        let dir = TempDir::new().unwrap();
        let mut tail = TranscriptTail::new(sample_path(&dir, "missing.jsonl"));
        let events = tail.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn empty_file_yields_no_events() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "empty.jsonl");
        fs::File::create(&path).await.unwrap();
        let mut tail = TranscriptTail::new(path);
        let events = tail.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn returns_each_complete_line_then_holds_partial() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        let mut tail = TranscriptTail::new(path.clone());

        // First write: two complete lines + a partial.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, r#"{{"i": 1}}"#).unwrap();
            writeln!(f, r#"{{"i": 2}}"#).unwrap();
            write!(f, r#"{{"i":"#).unwrap(); // partial
        }

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["i"], 1);
        assert_eq!(events[1]["i"], 2);

        // Second write: complete the partial + append another line.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, r#" 3}}"#).unwrap();
            writeln!(f, r#"{{"i": 4}}"#).unwrap();
        }

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["i"], 3);
        assert_eq!(events[1]["i"], 4);
    }

    #[tokio::test]
    async fn no_new_content_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"x\":1}\n").unwrap();
        let mut tail = TranscriptTail::new(path);

        let first = tail.poll().await.unwrap();
        assert_eq!(first.len(), 1);

        let second = tail.poll().await.unwrap();
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn truncation_resets_cursor_and_reemits() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let mut tail = TranscriptTail::new(path.clone());
        let first = tail.poll().await.unwrap();
        assert_eq!(first.len(), 2);

        // Truncate to a single short line.
        std::fs::write(&path, "{\"b\":3}\n").unwrap();
        let second = tail.poll().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["b"], 3);
    }

    #[tokio::test]
    async fn blank_lines_are_skipped() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "\n{\"x\":1}\n\n{\"x\":2}\n\n").unwrap();
        let mut tail = TranscriptTail::new(path);
        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn malformed_line_returns_json_error_with_line_text() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"ok\":1}\n{not json\n").unwrap();
        let mut tail = TranscriptTail::new(path);
        let result = tail.poll().await;
        match result {
            Err(TailError::Json { line, .. }) => assert_eq!(line, "{not json"),
            other => panic!("expected JSON error with line, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_appearing_after_initial_poll_is_picked_up() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "later.jsonl");
        let mut tail = TranscriptTail::new(path.clone());

        // First poll: file does not exist yet.
        assert!(tail.poll().await.unwrap().is_empty());

        // Now create it with content.
        let mut f = fs::File::create(&path).await.unwrap();
        f.write_all(b"{\"hello\":\"world\"}\n").await.unwrap();
        f.sync_all().await.unwrap();
        drop(f);

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["hello"], "world");
    }
}
