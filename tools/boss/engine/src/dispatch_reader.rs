//! Read-side companion to [`crate::dispatch_events`].
//!
//! The dispatch pipeline emits JSONL events into
//! `<state-root>/dispatch-events/current.jsonl` and per-execution
//! mirrors at `<state-root>/executions/<id>/dispatch.jsonl`. The
//! engine itself never reads those back — the writer is fire-and-
//! forget. This module is the read path that `bossctl dispatch tail`
//! / `diagnose` / `ghost-active` go through. It is deliberately
//! file-scan-only: it does NOT touch the engine RPC, so it works
//! when the engine is wedged.
//!
//! All readers are synchronous: the JSONL files are append-only and
//! small enough to scan in one pass per call. Each `read_current` /
//! `read_execution` returns a `Vec<DispatchEvent>` in the order they
//! were appended (lines that fail to parse are skipped with a
//! diagnostic on stderr — a half-written line at the tail of the
//! file is the common failure mode and we'd rather show what we have
//! than blow up).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::dispatch_events::{
    DispatchEvent, DispatchEventSink, Outcome as DispatchOutcome, Stage,
};

/// Per-stage stalled-detection thresholds. The watchdog used to apply
/// a single global threshold to every stage, but the cube-lease
/// hang incident (`exec_18aec07893bd2e30_29`, 2026-05-12) showed that
/// a 120s default is too coarse for the early dispatch stages — the
/// engine had wedged in `worker_claimed` for 46 seconds without any
/// `stage_stalled` event firing, because the global threshold hadn't
/// elapsed yet. Per-stage overrides let us flag the early handoffs
/// (worker_claimed → cube_repo_ensured → cube_workspace_leased)
/// faster while keeping the longer pane-spawn stages on a generous
/// threshold.
#[derive(Debug, Clone)]
pub struct StageThresholds {
    default_ms: u128,
    overrides: BTreeMap<String, u128>,
}

impl StageThresholds {
    pub fn new(default: Duration) -> Self {
        Self {
            default_ms: default.as_millis(),
            overrides: BTreeMap::new(),
        }
    }

    /// Override the threshold for a specific stage. Pass the wire
    /// stage name (e.g. `"worker_claimed"`) — the watchdog matches
    /// against `DispatchEvent::stage`.
    pub fn with_override(mut self, stage: impl Into<String>, threshold: Duration) -> Self {
        self.overrides.insert(stage.into(), threshold.as_millis());
        self
    }

    pub fn for_stage(&self, stage: &str) -> u128 {
        self.overrides
            .get(stage)
            .copied()
            .unwrap_or(self.default_ms)
    }

    pub fn default_ms(&self) -> u128 {
        self.default_ms
    }
}

/// Default Boss state root used by the file-scan readers when the
/// caller didn't override it. Mirrors the writer's default (see
/// [`crate::dispatch_events::JsonlFileSink`] callers in `app.rs`).
pub fn default_state_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss"))
}

/// Path to the flat dispatch-event stream under `root`.
pub fn current_path(root: &Path) -> PathBuf {
    root.join("dispatch-events").join("current.jsonl")
}

/// Path to the per-execution mirror under `root`.
pub fn execution_path(root: &Path, execution_id: &str) -> PathBuf {
    root.join("executions")
        .join(execution_id)
        .join("dispatch.jsonl")
}

/// Read every event currently in `current.jsonl`, in file order.
/// Missing file is treated as "no events" so callers can run against
/// a state root that hasn't been populated yet.
pub fn read_current(root: &Path) -> Result<Vec<DispatchEvent>> {
    read_jsonl(&current_path(root))
}

/// Read every event in the per-execution mirror for `execution_id`.
/// Missing file is treated as "no events".
pub fn read_execution(root: &Path, execution_id: &str) -> Result<Vec<DispatchEvent>> {
    read_jsonl(&execution_path(root, execution_id))
}

fn read_jsonl(path: &Path) -> Result<Vec<DispatchEvent>> {
    match fs::File::open(path) {
        Ok(file) => parse_lines(BufReader::new(file)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("opening {}", path.display())),
    }
}

fn parse_lines<R: BufRead>(reader: R) -> Result<Vec<DispatchEvent>> {
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading line {} from dispatch jsonl", idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<DispatchEvent>(&line) {
            Ok(event) => out.push(event),
            Err(err) => {
                eprintln!(
                    "warning: dropping unparseable dispatch event on line {}: {err}",
                    idx + 1
                );
            }
        }
    }
    Ok(out)
}

/// One entry in the `ghost-active` listing: an execution whose
/// dispatch timeline started but never reached a terminal stage
/// (`pane_spawned ok|error`, `run_started error`).
///
/// Surfaced by [`ghost_active`] for inspection through `bossctl
/// dispatch ghost-active`. Detection is event-shape only: we don't
/// need DB access to spot a timeline that just stops.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GhostActiveEntry {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub last_stage: String,
    pub last_outcome: String,
    pub last_ts_epoch_ms: u128,
    /// Milliseconds elapsed since the last event for this execution
    /// at the time `ghost_active` was called.
    pub elapsed_since_last_ms: u128,
    /// True when [`detect_stalled_stage`] flagged the timeline as
    /// stalled past `stalled_threshold_ms`.
    pub stalled: bool,
}

/// Return every per-execution timeline that hasn't reached a
/// terminal stage. `now_ms` is the wall-clock anchor used for
/// `elapsed_since_last_ms`; `stalled_threshold_ms` flips the
/// per-entry `stalled` field once the gap since the last event
/// exceeds it.
///
/// Scans `<root>/executions/<id>/dispatch.jsonl` for every
/// execution_id directory. The `current.jsonl` could be used too,
/// but the per-execution mirrors are cheaper to bucket and survive
/// rotation of the flat stream (if we ever add that).
pub fn ghost_active(
    root: &Path,
    now_ms: u128,
    stalled_threshold_ms: u128,
) -> Result<Vec<GhostActiveEntry>> {
    let executions_dir = root.join("executions");
    let read_dir = match fs::read_dir(&executions_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("opening {}", executions_dir.display()));
        }
    };

    let mut entries = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.with_context(|| format!("reading {}", executions_dir.display()))?;
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(execution_id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dispatch_path = path.join("dispatch.jsonl");
        if !dispatch_path.exists() {
            continue;
        }
        let events = read_jsonl(&dispatch_path)?;
        let Some(last) = events.last() else {
            continue;
        };
        if is_terminal_event(last) {
            continue;
        }
        let elapsed = now_ms.saturating_sub(last.ts_epoch_ms);
        entries.push(GhostActiveEntry {
            execution_id: execution_id.to_owned(),
            work_item_id: last.work_item_id.clone(),
            last_stage: last.stage.clone(),
            last_outcome: last.outcome.clone(),
            last_ts_epoch_ms: last.ts_epoch_ms,
            elapsed_since_last_ms: elapsed,
            stalled: elapsed >= stalled_threshold_ms,
        });
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.elapsed_since_last_ms));
    Ok(entries)
}

/// An event is "terminal" — i.e., the dispatch timeline for that
/// execution is officially over — when it is either a successful
/// `pane_spawned` (the slot is up and the worker is now driving),
/// or any explicit error (we won't get a follow-up; the
/// `record_start_failure` / `pane_spawn_failed` paths have run).
pub fn is_terminal_event(event: &DispatchEvent) -> bool {
    if event.outcome == "error" {
        return true;
    }
    if event.stage == "pane_spawned" && event.outcome == "ok" {
        return true;
    }
    false
}

/// Per-execution duration breakdown: time spent in each stage,
/// computed as `next_event.ts - this_event.ts`. The last stage's
/// duration is `now - last.ts` **only** when the last event is
/// non-terminal (see [`is_terminal_event`]); for terminal timelines
/// the final entry is `0` so the report doesn't grow forever after
/// dispatch has finished.
///
/// Used by `bossctl dispatch diagnose <id>` to render the timeline
/// without re-doing the math in main.
pub fn stage_durations_ms(events: &[DispatchEvent], now_ms: u128) -> Vec<u128> {
    let mut out = Vec::with_capacity(events.len());
    for i in 0..events.len() {
        let cur = events[i].ts_epoch_ms;
        let next = match events.get(i + 1) {
            Some(next_event) => next_event.ts_epoch_ms,
            None => {
                if is_terminal_event(&events[i]) {
                    cur
                } else {
                    now_ms
                }
            }
        };
        out.push(next.saturating_sub(cur));
    }
    out
}

/// Roll-up of (stage, outcome) → count over a slice of events.
/// `BTreeMap` so callers get stable ordering when they iterate.
pub fn count_by_stage_outcome(events: &[DispatchEvent]) -> BTreeMap<(String, String), usize> {
    let mut out = BTreeMap::new();
    for event in events {
        *out.entry((event.stage.clone(), event.outcome.clone()))
            .or_insert(0) += 1;
    }
    out
}

/// One stage stall the detector wants to surface as a
/// `stage_stalled` event. Carries enough context for the writer to
/// emit a fully-populated `DispatchEvent` without re-reading the
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledStage {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    /// The dispatch stage that hasn't progressed (e.g.
    /// `cube_change_created`). This is the last non-`stage_stalled`
    /// stage in the timeline — a previously-emitted stage_stalled
    /// event doesn't itself count as the stage that's stuck.
    pub stalled_stage: String,
    pub stalled_outcome: String,
    pub last_ts_epoch_ms: u128,
    pub elapsed_in_stage_ms: u128,
}

/// Walk every per-execution mirror under `root` and return the
/// stalls that haven't yet been surfaced. An execution is stalled
/// when its last "real" stage event (any non-`stage_stalled` event)
/// is non-terminal AND older than the per-stage threshold from
/// `thresholds`. To avoid duplicate `stage_stalled` lines for the
/// same wedge, we skip executions whose timeline already contains a
/// `stage_stalled` line referencing the current stalled stage.
pub fn pending_stalls(
    root: &Path,
    now_ms: u128,
    thresholds: &StageThresholds,
) -> Result<Vec<StalledStage>> {
    let executions_dir = root.join("executions");
    let read_dir = match fs::read_dir(&executions_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("opening {}", executions_dir.display()));
        }
    };

    let mut out = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.with_context(|| format!("reading {}", executions_dir.display()))?;
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(execution_id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dispatch_path = path.join("dispatch.jsonl");
        if !dispatch_path.exists() {
            continue;
        }
        let events = read_jsonl(&dispatch_path)?;
        let Some(stall) =
            stall_to_emit_for(execution_id, &events, now_ms, thresholds)
        else {
            continue;
        };
        out.push(stall);
    }
    Ok(out)
}

/// Convert a [`StalledStage`] into a fully-populated
/// `stage_stalled` dispatch event for the writer to emit. Kept here
/// (next to the detector) so the wire shape stays in one place.
pub fn build_stalled_event(stall: &StalledStage) -> DispatchEvent {
    let mut event = DispatchEvent::new(Stage::StageStalled, DispatchOutcome::Ok, &stall.execution_id);
    if let Some(work_item_id) = &stall.work_item_id {
        event = event.with_work_item(work_item_id.clone());
    }
    event.with_details(serde_json::json!({
        "stalled_stage": stall.stalled_stage,
        "stalled_outcome": stall.stalled_outcome,
        "stalled_at_ts_epoch_ms": stall.last_ts_epoch_ms as u64,
        "elapsed_in_stage_ms": stall.elapsed_in_stage_ms as u64,
    }))
}

/// Run one pass of [`pending_stalls`] and emit a `stage_stalled`
/// event per stall via `sink`. Designed to be called on a cadence by
/// [`spawn_stage_stalled_detector`].
pub async fn run_stage_stalled_pass(
    root: &Path,
    thresholds: &StageThresholds,
    sink: &dyn DispatchEventSink,
) -> Result<usize> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stalls = pending_stalls(root, now_ms, thresholds)?;
    let count = stalls.len();
    for stall in stalls {
        sink.emit(build_stalled_event(&stall)).await;
    }
    Ok(count)
}

/// Spawn a tokio task that runs [`run_stage_stalled_pass`] every
/// `interval`. The task has no shutdown path — engine process exit
/// drops the handle (same pattern as the merge poller).
pub fn spawn_stage_stalled_detector(
    root: PathBuf,
    sink: Arc<dyn DispatchEventSink>,
    thresholds: StageThresholds,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Stagger startup so an engine bring-up isn't paying for this
        // sweep while the rest of init is still running.
        tokio::time::sleep(interval).await;
        loop {
            match run_stage_stalled_pass(&root, &thresholds, sink.as_ref()).await {
                Ok(emitted) if emitted > 0 => {
                    tracing::info!(
                        emitted,
                        default_threshold_ms = thresholds.default_ms() as u64,
                        "stage_stalled detector: emitted events",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(?err, "stage_stalled detector: sweep failed");
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Reusable per-timeline core for [`pending_stalls`]. Returns the
/// `StalledStage` the caller should emit for `events`, or `None`
/// when the timeline is fresh, terminal, or already has a
/// `stage_stalled` line covering the current stalled stage.
fn stall_to_emit_for(
    execution_id: &str,
    events: &[DispatchEvent],
    now_ms: u128,
    thresholds: &StageThresholds,
) -> Option<StalledStage> {
    let last_real = events.iter().rev().find(|e| e.stage != "stage_stalled")?;
    if is_terminal_event(last_real) {
        return None;
    }
    let elapsed = now_ms.saturating_sub(last_real.ts_epoch_ms);
    let threshold_ms = thresholds.for_stage(&last_real.stage);
    if elapsed < threshold_ms {
        return None;
    }
    let already_flagged = events.iter().any(|e| {
        e.stage == "stage_stalled"
            && e.details
                .get("stalled_stage")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == last_real.stage)
            && e.details
                .get("stalled_at_ts_epoch_ms")
                .and_then(|v| v.as_u64())
                .is_some_and(|t| t as u128 == last_real.ts_epoch_ms)
    });
    if already_flagged {
        return None;
    }
    Some(StalledStage {
        execution_id: execution_id.to_owned(),
        work_item_id: last_real.work_item_id.clone(),
        stalled_stage: last_real.stage.clone(),
        stalled_outcome: last_real.outcome.clone(),
        last_ts_epoch_ms: last_real.ts_epoch_ms,
        elapsed_in_stage_ms: elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::{DispatchEvent, JsonlFileSink, Outcome, Stage};
    use tempfile::TempDir;

    async fn write(sink: &JsonlFileSink, ev: DispatchEvent) {
        use crate::dispatch_events::DispatchEventSink;
        sink.emit(ev).await;
    }

    #[tokio::test]
    async fn read_current_returns_events_in_file_order() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"),
        )
        .await;
        write(
            &sink,
            DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-1"),
        )
        .await;
        write(
            &sink,
            DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-1"),
        )
        .await;

        let events = read_current(dir.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].stage, "request_recorded");
        assert_eq!(events[2].stage, "pane_spawned");
    }

    #[tokio::test]
    async fn read_execution_filters_to_one_mirror() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-a"),
        )
        .await;
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-b"),
        )
        .await;
        write(
            &sink,
            DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-a"),
        )
        .await;

        let a = read_execution(dir.path(), "exec-a").unwrap();
        let b = read_execution(dir.path(), "exec-b").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn read_current_on_missing_root_yields_empty() {
        let dir = TempDir::new().unwrap();
        let events = read_current(dir.path()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_lines_skips_blank_and_unparseable() {
        let input = b"\n\
            {\"ts_epoch_ms\":1,\"stage\":\"request_recorded\",\"outcome\":\"ok\",\"execution_id\":\"e\",\"details\":null}\n\
            not-a-json-line\n\
            {\"ts_epoch_ms\":2,\"stage\":\"worker_claimed\",\"outcome\":\"ok\",\"execution_id\":\"e\",\"details\":null}\n";
        let events = parse_lines(std::io::BufReader::new(&input[..])).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn ghost_active_lists_executions_with_non_terminal_last_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // exec-stuck: stops at cube_change_created (non-terminal)
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-stuck"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        event.ts_epoch_ms = 1000;
        write(&sink, event).await;

        // exec-ok: reaches pane_spawned ok → not ghost-active
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-ok"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-ok");
        event.ts_epoch_ms = 2000;
        write(&sink, event).await;

        // exec-failed: reaches run_started error → not ghost-active
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-failed"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::RunStarted, Outcome::Error, "exec-failed");
        event.ts_epoch_ms = 3000;
        write(&sink, event).await;

        let entries = ghost_active(dir.path(), 10_000, 5_000).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].execution_id, "exec-stuck");
        assert_eq!(entries[0].last_stage, "cube_change_created");
        assert_eq!(entries[0].elapsed_since_last_ms, 9_000);
        assert!(entries[0].stalled);
    }

    #[tokio::test]
    async fn ghost_active_stalled_flag_respects_threshold() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // event at t=9000, now=10000 → elapsed=1000 → not stalled
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-fresh"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-fresh");
        event.ts_epoch_ms = 9_000;
        write(&sink, event).await;

        let entries = ghost_active(dir.path(), 10_000, 5_000).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].elapsed_since_last_ms, 1_000);
        assert!(!entries[0].stalled);
    }

    #[tokio::test]
    async fn stage_durations_ms_uses_now_for_last_non_terminal_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        a.ts_epoch_ms = 100;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "e");
        b.ts_epoch_ms = 250;
        write(&sink, b).await;
        let mut c = DispatchEvent::new(Stage::CubeRepoEnsured, Outcome::Ok, "e");
        c.ts_epoch_ms = 700;
        write(&sink, c).await;

        let events = read_execution(dir.path(), "e").unwrap();
        let durations = stage_durations_ms(&events, 1_500);
        assert_eq!(durations, vec![150, 450, 800]);
    }

    #[tokio::test]
    async fn stage_durations_ms_uses_zero_for_terminal_last_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        a.ts_epoch_ms = 100;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "e");
        b.ts_epoch_ms = 250;
        write(&sink, b).await;

        let events = read_execution(dir.path(), "e").unwrap();
        let durations = stage_durations_ms(&events, 9_999_999);
        // Terminal event => duration is 0, not 9_999_999 - 250.
        assert_eq!(durations, vec![150, 0]);
    }

    fn flat_thresholds(ms: u64) -> StageThresholds {
        StageThresholds::new(Duration::from_millis(ms))
    }

    #[tokio::test]
    async fn pending_stalls_emits_when_threshold_passed_and_no_prior_flag() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        // Fresh request_recorded then a CubeChangeCreated that hasn't
        // moved on — past the 5s threshold at now=10s.
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-stuck");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        b.ts_epoch_ms = 1_000;
        write(&sink, b).await;

        let stalls = pending_stalls(dir.path(), 10_000, &flat_thresholds(5_000)).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-stuck");
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
        assert_eq!(stalls[0].elapsed_in_stage_ms, 9_000);
    }

    #[tokio::test]
    async fn pending_stalls_skips_terminal_timelines() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        // pane_spawned: ok is terminal.
        let mut a = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-done");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;

        let stalls = pending_stalls(dir.path(), 9_999_999, &flat_thresholds(5_000)).unwrap();
        assert!(stalls.is_empty());
    }

    #[tokio::test]
    async fn pending_stalls_skips_executions_already_flagged_for_the_same_stage() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        let mut a = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-flagged");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;
        let mut flag = DispatchEvent::new(Stage::StageStalled, Outcome::Ok, "exec-flagged");
        flag.ts_epoch_ms = 6_500;
        flag.details = serde_json::json!({
            "stalled_stage": "cube_change_created",
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": 1_000,
        });
        write(&sink, flag).await;

        let stalls = pending_stalls(dir.path(), 10_000, &flat_thresholds(5_000)).unwrap();
        assert!(stalls.is_empty(), "got {stalls:?}");
    }

    #[tokio::test]
    async fn pending_stalls_re_emits_when_stage_advances() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // Stalled at cube_workspace_leased, already flagged.
        let mut a = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-x");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;
        let mut flag = DispatchEvent::new(Stage::StageStalled, Outcome::Ok, "exec-x");
        flag.ts_epoch_ms = 6_500;
        flag.details = serde_json::json!({
            "stalled_stage": "cube_workspace_leased",
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": 1_000,
        });
        write(&sink, flag).await;

        // Now stage advances to cube_change_created, but stalls again.
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-x");
        b.ts_epoch_ms = 7_000;
        write(&sink, b).await;

        // At now=15s the new stage has been stuck for 8s.
        let stalls = pending_stalls(dir.path(), 15_000, &flat_thresholds(5_000)).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
    }

    #[tokio::test]
    async fn pending_stalls_honours_per_stage_threshold_overrides() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // worker_claimed at t=0, now=35_000 → 35s in stage. Default
        // threshold is 120s (would NOT fire), but worker_claimed has
        // a 30s override → should fire.
        let mut a = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-claimed");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;

        // pane_spawned-style longer stage: cube_change_created at
        // t=0 (also 35s elapsed) but no override → falls under the
        // 120s default and should NOT fire.
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-changing");
        b.ts_epoch_ms = 0;
        write(&sink, b).await;

        let thresholds = StageThresholds::new(Duration::from_secs(120))
            .with_override("worker_claimed", Duration::from_secs(30));
        let stalls = pending_stalls(dir.path(), 35_000, &thresholds).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-claimed");
        assert_eq!(stalls[0].stalled_stage, "worker_claimed");
    }

    #[test]
    fn stage_thresholds_falls_back_to_default_for_unknown_stages() {
        let t = StageThresholds::new(Duration::from_secs(120))
            .with_override("worker_claimed", Duration::from_secs(30));
        assert_eq!(t.for_stage("worker_claimed"), 30_000);
        assert_eq!(t.for_stage("cube_repo_ensured"), 120_000);
        assert_eq!(t.for_stage("anything_else"), 120_000);
    }

    #[test]
    fn is_terminal_event_recognises_terminal_shapes() {
        let req = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        assert!(!is_terminal_event(&req));
        let pane_ok = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "e");
        assert!(is_terminal_event(&pane_ok));
        let run_err = DispatchEvent::new(Stage::RunStarted, Outcome::Error, "e");
        assert!(is_terminal_event(&run_err));
        let pane_err = DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "e");
        assert!(is_terminal_event(&pane_err));
    }
}
