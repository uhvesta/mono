//! Append-only JSONL log of every IPC exchange between the engine and
//! the macOS app on the Unix socket. Rotates daily; retains the last
//! N days (default 7). Writes are dispatched to a background task so
//! the hot path (send_to_app / deliver_app_response) is never blocked
//! on disk I/O.
//!
//! Log lives at: `<boss-state-root>/ipc/ipc-YYYY-MM-DD.jsonl`
//!
//! Each line is a JSON object:
//!   `ts_epoch_ms`  – milliseconds since Unix epoch
//!   `direction`    – `"engine→app"` or `"app→engine"`
//!   `request_id`   – opaque id that pairs a request with its response
//!   `kind`         – snake_case discriminant (e.g. `"release_worker_pane"`)
//!   `body`         – the full serialised request or response payload

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::protocol::{EngineToAppRequest, EngineToAppResponse};

const RETAIN_DAYS: u64 = 7;

#[derive(Debug, Serialize)]
struct IpcLogEntry {
    ts_epoch_ms: u128,
    direction: &'static str,
    request_id: String,
    kind: &'static str,
    body: Value,
}

/// Async-safe, append-only IPC log writer.
///
/// Calls to [`log_request`] and [`log_response`] are non-blocking:
/// entries are sent over an in-process channel to a background task
/// that owns the file handles and performs all I/O.
pub struct IpcLogger {
    tx: mpsc::UnboundedSender<IpcLogEntry>,
}

impl IpcLogger {
    /// Create a new logger that writes under `<root>/ipc/`.
    /// Spawns a Tokio background task when a runtime is available.
    /// When called outside a Tokio runtime (e.g. synchronous unit tests),
    /// the channel is created but the writer task is not spawned — log
    /// entries queue up and are silently dropped when the sender is dropped.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(writer_task(root.into(), rx));
        }
        Self { tx }
    }

    /// Log an outbound request (engine → app).
    pub fn log_request(&self, request_id: &str, request: &EngineToAppRequest) {
        self.send(IpcLogEntry {
            ts_epoch_ms: now_ms(),
            direction: "engine→app",
            request_id: request_id.to_owned(),
            kind: request_kind(request),
            body: serde_json::to_value(request).unwrap_or(Value::Null),
        });
    }

    /// Log an inbound response (app → engine).
    pub fn log_response(&self, request_id: &str, response: &EngineToAppResponse) {
        self.send(IpcLogEntry {
            ts_epoch_ms: now_ms(),
            direction: "app→engine",
            request_id: request_id.to_owned(),
            kind: response_kind(response),
            body: serde_json::to_value(response).unwrap_or(Value::Null),
        });
    }

    fn send(&self, entry: IpcLogEntry) {
        // Fire-and-forget. If the receiver is gone (task exited), drop silently.
        let _ = self.tx.send(entry);
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn request_kind(req: &EngineToAppRequest) -> &'static str {
    match req {
        EngineToAppRequest::SpawnWorkerPane(_) => "spawn_worker_pane",
        EngineToAppRequest::ReleaseWorkerPane(_) => "release_worker_pane",
        EngineToAppRequest::SendToPane(_) => "send_to_pane",
        EngineToAppRequest::FocusWorkerPane(_) => "focus_worker_pane",
        EngineToAppRequest::InterruptWorkerPane(_) => "interrupt_worker_pane",
        EngineToAppRequest::RevealWorkItem(_) => "reveal_work_item",
    }
}

fn response_kind(resp: &EngineToAppResponse) -> &'static str {
    match resp {
        EngineToAppResponse::SpawnWorkerPane { .. } => "spawn_worker_pane",
        EngineToAppResponse::ReleaseWorkerPane { .. } => "release_worker_pane",
        EngineToAppResponse::SendToPane { .. } => "send_to_pane",
        EngineToAppResponse::FocusWorkerPane { .. } => "focus_worker_pane",
        EngineToAppResponse::InterruptWorkerPane { .. } => "interrupt_worker_pane",
        EngineToAppResponse::RevealWorkItem { .. } => "reveal_work_item",
    }
}

async fn writer_task(root: PathBuf, mut rx: mpsc::UnboundedReceiver<IpcLogEntry>) {
    use std::io::Write;

    let ipc_dir = root.join("ipc");
    let mut current_date = String::new();
    let mut file: Option<std::fs::File> = None;

    while let Some(entry) = rx.recv().await {
        let date_str = epoch_ms_to_date(entry.ts_epoch_ms);

        if date_str != current_date {
            // Date rolled over: close the old file and prune old logs.
            file = None;
            prune_old_files(&ipc_dir, RETAIN_DAYS);
        }

        if file.is_none() {
            if let Err(err) = std::fs::create_dir_all(&ipc_dir) {
                tracing::warn!(?err, "ipc_log: failed to create ipc dir; dropping entry");
                continue;
            }
            let path = ipc_dir.join(format!("ipc-{date_str}.jsonl"));
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(f) => {
                    file = Some(f);
                    current_date = date_str;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = %path.display(),
                        "ipc_log: failed to open log file; dropping entry"
                    );
                    continue;
                }
            }
        }

        let Some(ref mut f) = file else { continue };
        match serde_json::to_vec(&entry) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if let Err(err) = f.write_all(&bytes) {
                    tracing::warn!(?err, "ipc_log: write failed; dropping entry");
                }
            }
            Err(err) => {
                tracing::warn!(?err, "ipc_log: serialization failed; dropping entry");
            }
        }
    }
}

fn prune_old_files(dir: &Path, keep_days: u64) {
    let cutoff_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        .saturating_sub(u128::from(keep_days) * 86_400_000);
    let cutoff_date = epoch_ms_to_date(cutoff_ms);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(date_part) = name
            .strip_prefix("ipc-")
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            if date_part < cutoff_date.as_str() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn epoch_ms_to_date(ms: u128) -> String {
    let secs = (ms / 1000) as u64;
    let (y, mo, da) = days_to_ymd((secs / 86400) as i64);
    format!("{y:04}-{mo:02}-{da:02}")
}

/// Civil (Gregorian) date from days since the Unix epoch (1970-01-01).
/// Algorithm: <http://www.howardhinnant.com/date_algorithms.html#civil_from_days>
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        EngineToAppResponse, ReleaseWorkerPaneInput, ReleaseWorkerPaneResult,
    };

    #[test]
    fn epoch_ms_to_date_known_values() {
        // 2026-05-14 00:00:00 UTC = 1 778 716 800 seconds
        let ms = 1_778_716_800_000u128;
        assert_eq!(epoch_ms_to_date(ms), "2026-05-14");
        assert_eq!(epoch_ms_to_date(ms + 43_200_000), "2026-05-14"); // noon same day
        assert_eq!(epoch_ms_to_date(ms + 86_400_000), "2026-05-15"); // next day
    }

    #[test]
    fn days_to_ymd_known_values() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(1), (1970, 1, 2));
        assert_eq!(days_to_ymd(365), (1971, 1, 1));
        // 2026-05-14 = day 20587 since Unix epoch
        assert_eq!(days_to_ymd(20_587), (2026, 5, 14));
    }

    #[tokio::test]
    async fn ipc_logger_writes_and_rotates() {
        let dir = tempfile::TempDir::new().unwrap();
        let logger = IpcLogger::new(dir.path());

        let req = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id: 3,
            kill_grace_seconds: 5,
        });
        logger.log_request("eng-req-42", &req);

        let resp = EngineToAppResponse::ReleaseWorkerPane {
            result: Ok(ReleaseWorkerPaneResult {}),
        };
        logger.log_response("eng-req-42", &resp);

        // Let the background task flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let ipc_dir = dir.path().join("ipc");
        let mut files: Vec<_> = std::fs::read_dir(&ipc_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "one daily log file");

        let content = std::fs::read_to_string(&files[0]).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let req_entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(req_entry["direction"], "engine→app");
        assert_eq!(req_entry["kind"], "release_worker_pane");
        assert_eq!(req_entry["request_id"], "eng-req-42");
        assert!(req_entry["ts_epoch_ms"].is_number());

        let resp_entry: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp_entry["direction"], "app→engine");
        assert_eq!(resp_entry["kind"], "release_worker_pane");
        assert_eq!(resp_entry["request_id"], "eng-req-42");
    }

    #[test]
    fn prune_old_files_removes_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let ipc_dir = dir.path().join("ipc");
        std::fs::create_dir_all(&ipc_dir).unwrap();

        // Create a file 8 days in the past (should be pruned).
        let old_ms = now_ms().saturating_sub(8 * 86_400_000);
        let old_date = epoch_ms_to_date(old_ms);
        let old_path = ipc_dir.join(format!("ipc-{old_date}.jsonl"));
        std::fs::write(&old_path, b"old\n").unwrap();

        // Create a file 3 days in the past (should be kept).
        let recent_ms = now_ms().saturating_sub(3 * 86_400_000);
        let recent_date = epoch_ms_to_date(recent_ms);
        let recent_path = ipc_dir.join(format!("ipc-{recent_date}.jsonl"));
        std::fs::write(&recent_path, b"recent\n").unwrap();

        prune_old_files(&ipc_dir, 7);

        assert!(!old_path.exists(), "8-day-old file should be pruned");
        assert!(recent_path.exists(), "3-day-old file should be kept");
    }
}
