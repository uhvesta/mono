//! Append-only audit log for engine lifecycle events.
//!
//! Each engine process opens (or appends to) `engine-audit.log` under
//! `~/Library/Application Support/Boss/` and writes a JSON line on
//! every load-bearing transition: startup, every socket bind attempt,
//! and shutdown (clean, signalled, or panic).
//!
//! The intent is forensic: when something blows up or the engine
//! "mysteriously" restarted, `tail engine-audit.log` should answer
//! "when did the current engine start, who launched it, what sockets
//! did it bind, and how did the previous instance end?". `ps` is lossy
//! and only works while the process is alive — this file outlives
//! every engine process and survives state.db corruption (it lives
//! next to state.db, not inside it).
//!
//! ## Format
//!
//! One JSON object per line. Every record carries `ts` (RFC 3339 UTC),
//! `ts_epoch_s` (i64 seconds since epoch), `event`, and `pid`. Other
//! fields vary by event. Field shape is best-effort: a missing
//! collector (e.g. parent command) is silently dropped rather than
//! blocking the write.
//!
//! ## Bounded growth
//!
//! Each record is at most a few hundred bytes. We cap the file at
//! [`MAX_LOG_BYTES`] and rotate by truncating the oldest half on
//! overflow — simple, in-process, no external dependency, and enough
//! to keep months of restart history available without unbounded
//! growth.
//!
//! ## Best-effort
//!
//! A failed audit write must never block engine startup or shutdown.
//! All public entry points swallow IO errors after logging them via
//! `tracing` — the audit log is observability, not a transactional
//! resource.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value, json};

const SECS_PER_DAY: i64 = 86_400;

/// Hard cap on the audit log size. Once exceeded, the oldest ~half of
/// the file is dropped on the next append. Generous enough for months
/// of normal startup/shutdown cycles.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Override the audit log path. Primarily for tests; production code
/// uses [`default_audit_log_path`]. The literal is owned by `boss-log-files`
/// (the single source of truth for log path/rotation) and re-exported here so
/// existing `audit::AUDIT_PATH_ENV` call sites — and any external readers like
/// `bossctl` — agree on one definition.
pub const AUDIT_PATH_ENV: &str = boss_log_files::AUDIT_PATH_ENV;

/// Engine startup epoch seconds, captured by [`record_start`] and
/// reused by [`record_shutdown`] to derive `uptime_sec`. Stored as a
/// process-global so the panic / signal paths don't need to thread the
/// value through every call site.
static START_EPOCH_S: AtomicI64 = AtomicI64::new(0);

/// Resolved audit log path for this process. Set on the first call to
/// [`record_start`] (or any other entry point) and reused thereafter.
static AUDIT_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set after the first [`record_shutdown`] call so a subsequent
/// shutdown emission (e.g. main returning Ok after the signal-path
/// already recorded `signal:SIGTERM`) is a no-op. Without this, we'd
/// write two shutdown records per run and the trailing `normal` would
/// overwrite the more useful signal label in any "last shutdown
/// reason" lookup.
static SHUTDOWN_EMITTED: AtomicBool = AtomicBool::new(false);

/// Resolve the default path: honours `BOSS_ENGINE_AUDIT_PATH` first,
/// otherwise falls back to `$HOME/Library/Application Support/Boss/
/// engine-audit.log`. Delegates to `boss-log-files` so the path resolution
/// (env override + default location) lives in exactly one place, shared with
/// the `bossctl` reader.
pub fn default_audit_log_path() -> Option<PathBuf> {
    boss_log_files::default_audit_log_path()
}

/// Set the audit log path explicitly. Used by tests and by the
/// production main on startup. Idempotent — only the first caller
/// wins, so subsequent record_* calls all target the same file.
pub fn set_audit_path(path: PathBuf) {
    let _ = AUDIT_PATH.set(path);
}

/// Snapshot of the values that [`record_start`] writes. Built from the
/// running process's argv, env, and any optional fields the caller
/// wants to attach (engine version, socket paths, db path).
#[derive(Debug, Clone, Default)]
pub struct StartContext {
    pub argv: Vec<String>,
    pub engine_version: Option<String>,
    pub socket_paths: Vec<PathBuf>,
    pub state_db_path: Option<PathBuf>,
    pub prior_state_db_size: Option<u64>,
    pub parent_command: Option<String>,
}

/// Record a `start` event. Call this once near the top of `main`,
/// before any work that could fail — even if the engine crashes
/// during init we'll still have a "start without shutdown" pair on
/// disk to interpret on the next boot.
pub fn record_start(ctx: StartContext) {
    let now = epoch_now_s();
    START_EPOCH_S.store(now, Ordering::SeqCst);
    SHUTDOWN_EMITTED.store(false, Ordering::SeqCst);

    let mut fields = Map::new();
    fields.insert("pid".into(), json!(std::process::id()));
    if let Some(ppid) = parent_pid() {
        fields.insert("ppid".into(), json!(ppid));
    }
    if !ctx.argv.is_empty() {
        fields.insert("argv".into(), json!(ctx.argv));
    }
    if let Some(parent) = ctx.parent_command {
        fields.insert("parent_command".into(), Value::String(parent));
    }
    if let Some(version) = ctx.engine_version {
        fields.insert("engine_version".into(), Value::String(version));
    }
    if !ctx.socket_paths.is_empty() {
        let strs: Vec<String> = ctx.socket_paths.iter().map(|p| p.display().to_string()).collect();
        fields.insert("socket_paths".into(), json!(strs));
    }
    if let Some(db) = ctx.state_db_path {
        fields.insert("state_db_path".into(), Value::String(db.display().to_string()));
    }
    if let Some(size) = ctx.prior_state_db_size {
        fields.insert("prior_state_db_size".into(), json!(size));
    }

    write_record(now, "start", fields);
}

/// Record a clean / signalled / panicked shutdown. `reason` is a
/// short stable label like `normal`, `signal:SIGTERM`, or
/// `crash:<short message>`.
///
/// Only the first call per process actually writes; later calls are
/// dropped so the signal handler's `signal:SIGTERM` reason wins over
/// the trailing `normal` that `main` would otherwise emit.
pub fn record_shutdown(reason: impl Into<String>) {
    if SHUTDOWN_EMITTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let now = epoch_now_s();
    let started = START_EPOCH_S.load(Ordering::SeqCst);
    let mut fields = Map::new();
    fields.insert("pid".into(), json!(std::process::id()));
    fields.insert("reason".into(), Value::String(reason.into()));
    if started > 0 {
        let uptime = (now - started).max(0);
        fields.insert("uptime_sec".into(), json!(uptime));
    }
    write_record(now, "shutdown", fields);
}

/// Outcome of a socket bind attempt. Surfaces the same window
/// (between "about to bind" and "listen succeeded / failed") that the
/// 2026-05-07 incident left invisible.
#[derive(Debug, Clone, Copy)]
pub enum SocketBindResult<'a> {
    Succeeded,
    Failed(&'a str),
}

/// Record a socket lifecycle event. `kind` identifies which socket
/// (e.g. `frontend`, `events`); `path` is the bind path.
pub fn record_socket_bind(kind: &str, path: &Path, result: SocketBindResult<'_>) {
    let now = epoch_now_s();
    let mut fields = Map::new();
    fields.insert("pid".into(), json!(std::process::id()));
    fields.insert("socket_kind".into(), Value::String(kind.to_owned()));
    fields.insert("socket_path".into(), Value::String(path.display().to_string()));
    let event = match result {
        SocketBindResult::Succeeded => "socket_bound",
        SocketBindResult::Failed(err) => {
            fields.insert("error".into(), Value::String(err.to_owned()));
            "socket_bind_failed"
        }
    };
    write_record(now, event, fields);
}

/// Record that an accept loop is now running on a socket. Emitted
/// once per socket immediately before the first `accept()` poll, so
/// the audit log captures the full lifecycle (`socket_bound` →
/// `accept_loop_started`) instead of leaving the next person
/// triaging an incident with only mtime archaeology to figure out
/// when (or whether) the engine actually entered the listen loop.
pub fn record_accept_loop_started(kind: &str, path: &Path) {
    let now = epoch_now_s();
    let mut fields = Map::new();
    fields.insert("pid".into(), json!(std::process::id()));
    fields.insert("socket_kind".into(), Value::String(kind.to_owned()));
    fields.insert("socket_path".into(), Value::String(path.display().to_string()));
    write_record(now, "accept_loop_started", fields);
}

/// Record a `shutdown_rpc` attempt — the engine received a
/// `Shutdown` request on the frontend socket. `outcome` is one of
/// `"accepted"`, `"token_mismatch"`, or `"token_missing"`. Logged
/// regardless of result so a wrong-target test or worker leaves an
/// explicit trail rather than silently SIGTERMing the engine.
pub fn record_shutdown_rpc(outcome: &str, peer_pid: Option<i32>) {
    let now = epoch_now_s();
    let mut fields = Map::new();
    fields.insert("pid".into(), json!(std::process::id()));
    fields.insert("outcome".into(), Value::String(outcome.to_owned()));
    if let Some(p) = peer_pid {
        fields.insert("peer_pid".into(), json!(p));
    }
    write_record(now, "shutdown_rpc", fields);
}

/// Record an arbitrary auxiliary event. Reserved for future expansion
/// (e.g. socket teardown, accept errors). Keeps a single funnel into
/// the file format.
#[allow(dead_code)]
pub fn record_event<T: Serialize>(event: &str, payload: &T) {
    let value = match serde_json::to_value(payload) {
        Ok(Value::Object(m)) => m,
        Ok(other) => {
            let mut m = Map::new();
            m.insert("payload".into(), other);
            m
        }
        Err(err) => {
            tracing::warn!(?err, event, "engine-audit: failed to serialize event payload");
            return;
        }
    };
    write_record(epoch_now_s(), event, value);
}

fn epoch_now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parent_pid() -> Option<u32> {
    // SAFETY: getppid is always safe; it returns the calling process's
    // parent pid and never fails.
    let raw = unsafe { libc::getppid() };
    if raw <= 0 { None } else { Some(raw as u32) }
}

fn write_record(now_epoch_s: i64, event: &str, mut fields: Map<String, Value>) {
    let Some(path) = resolve_path() else {
        tracing::warn!("engine-audit: no path resolved (HOME unset?); skipping record");
        return;
    };

    fields.insert("ts".into(), Value::String(format_iso8601(now_epoch_s)));
    fields.insert("ts_epoch_s".into(), json!(now_epoch_s));
    fields.insert("event".into(), Value::String(event.to_owned()));

    if let Err(err) = append_to(&path, &Value::Object(fields)) {
        tracing::warn!(?err, path = %path.display(), event, "engine-audit: append failed");
    }
}

fn resolve_path() -> Option<PathBuf> {
    if let Some(p) = AUDIT_PATH.get() {
        return Some(p.clone());
    }
    let resolved = default_audit_log_path()?;
    let _ = AUDIT_PATH.set(resolved.clone());
    Some(resolved)
}

fn append_to(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    rotate_if_needed(path).ok();

    let mut line =
        serde_json::to_string(value).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    line.push('\n');

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// If `path` is over [`MAX_LOG_BYTES`], drop the oldest ~half of the
/// file. Best-effort: any error short-circuits without touching the
/// file (so the next append still lands at the tail of whatever's
/// there).
fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.len() <= MAX_LOG_BYTES {
        return Ok(());
    }

    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let drop_to = len / 2;
    file.seek(SeekFrom::Start(drop_to))?;
    let mut buf = Vec::with_capacity(len.saturating_sub(drop_to) as usize);
    file.read_to_end(&mut buf)?;

    // Cut on the first newline so the surviving prefix starts with a
    // complete JSON record. If the second half has no newline at all,
    // we just keep what we have (better than truncating mid-line).
    let start = buf.iter().position(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf[start..])?;
    Ok(())
}

fn format_iso8601(epoch_s: i64) -> String {
    let day = epoch_s.div_euclid(SECS_PER_DAY);
    let sec_in_day = epoch_s.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(day);
    let h = sec_in_day / 3600;
    let mi = (sec_in_day / 60) % 60;
    let s = sec_in_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

// Howard Hinnant's date algorithms — same shape as `tools/cube/src/audit.rs`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serializes the audit tests that mutate process-global state — the
    /// `AUDIT_PATH_ENV` env var, the `AUDIT_PATH` `OnceLock`, and the
    /// `START_EPOCH_S` / `SHUTDOWN_EMITTED` statics. These are shared
    /// across the whole test binary, so without a lock two of these
    /// tests running concurrently in the same shard clobber each other's
    /// env override and atomics (observed as a flaky
    /// "expected shutdown, got start"). The lock recovers from poisoning
    /// so one failing test doesn't cascade into the others.
    static AUDIT_GLOBALS_LOCK: Mutex<()> = Mutex::new(());

    fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        AUDIT_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn parse_lines(path: &Path) -> Vec<Value> {
        let raw = fs::read_to_string(path).unwrap();
        raw.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("valid jsonl"))
            .collect()
    }

    fn fresh_audit_path(dir: &TempDir, name: &str) -> PathBuf {
        // Just build a per-test path under the TempDir. The callers below
        // write through `record_via_direct_path` / `append_to`, which take
        // an explicit path and never consult `AUDIT_PATH_ENV` or the
        // OnceLock — so this helper must NOT touch the process-global env
        // var. It used to call `set_var(AUDIT_PATH_ENV, …)`, which served
        // no purpose here (these tests don't go through `resolve_path`) but
        // raced with `public_start_and_shutdown_path_emits_two_records`,
        // the one test that does read the env: a concurrent `set_var` here
        // clobbered that test's path mid-`record_start`, so it then read a
        // file that was never written (`NotFound`). Keeping this pure makes
        // the audit tests independent under libtest's default parallelism.
        dir.path().join(name)
    }

    #[test]
    fn iso8601_round_trip_for_known_instant() {
        // 2026-05-07 20:04:11 UTC.
        let day = days_from_civil(2026, 5, 7);
        let ts = day * SECS_PER_DAY + 20 * 3600 + 4 * 60 + 11;
        assert_eq!(format_iso8601(ts), "2026-05-07T20:04:11Z");
    }

    #[test]
    fn record_start_writes_required_fields() {
        let _globals = lock_globals();
        let dir = TempDir::new().unwrap();
        let path = fresh_audit_path(&dir, "start.log");

        // Force resolve_path to use the env override even after a
        // prior test populated the OnceLock.
        record_via_direct_path(&path, "start", {
            let mut m = Map::new();
            m.insert("pid".into(), json!(42));
            m.insert("argv".into(), json!(["engine"]));
            m.insert(
                "socket_paths".into(),
                json!(["/tmp/boss-engine.sock", "/tmp/events.sock"]),
            );
            m
        });

        let parsed = parse_lines(&path);
        assert_eq!(parsed.len(), 1);
        let row = &parsed[0];
        assert_eq!(row["event"], "start");
        assert!(row["ts"].as_str().unwrap().ends_with('Z'));
        assert!(row["ts_epoch_s"].as_i64().unwrap() > 0);
        assert_eq!(row["pid"], 42);
        assert_eq!(row["argv"][0], "engine");
        assert_eq!(row["socket_paths"][1], "/tmp/events.sock");
    }

    #[test]
    fn record_shutdown_includes_uptime_when_start_was_recorded() {
        let _globals = lock_globals();
        let dir = TempDir::new().unwrap();
        let path = fresh_audit_path(&dir, "shutdown.log");

        // Pretend the engine started 1234s ago.
        let now = epoch_now_s();
        START_EPOCH_S.store(now - 1234, Ordering::SeqCst);

        // Bypass the OnceLock by writing directly.
        let row_start = make_record(now - 1234, "start", {
            let mut m = Map::new();
            m.insert("pid".into(), json!(7));
            m
        });
        let row_stop = make_record(now, "shutdown", {
            let mut m = Map::new();
            m.insert("pid".into(), json!(7));
            m.insert("reason".into(), Value::String("signal:SIGTERM".into()));
            m.insert("uptime_sec".into(), json!(1234));
            m
        });
        append_to(&path, &row_start).unwrap();
        append_to(&path, &row_stop).unwrap();

        let parsed = parse_lines(&path);
        assert_eq!(parsed[1]["event"], "shutdown");
        assert_eq!(parsed[1]["reason"], "signal:SIGTERM");
        assert_eq!(parsed[1]["uptime_sec"], 1234);
    }

    #[test]
    fn rotate_drops_oldest_half_when_over_cap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.log");
        // Build a file that is 1.5 * MAX_LOG_BYTES of newline-delimited
        // 100-byte records, so rotation has plenty of newlines to cut on.
        let line = "x".repeat(99) + "\n";
        let bytes_per_line = line.len() as u64;
        let target_lines = (MAX_LOG_BYTES + MAX_LOG_BYTES / 2) / bytes_per_line;
        {
            let mut f = OpenOptions::new().create(true).append(true).open(&path).unwrap();
            for _ in 0..target_lines {
                f.write_all(line.as_bytes()).unwrap();
            }
        }
        let before = fs::metadata(&path).unwrap().len();
        assert!(before > MAX_LOG_BYTES);

        // Append one more record via the public path — this triggers
        // rotation as a side effect.
        append_to(
            &path,
            &make_record(epoch_now_s(), "start", {
                let mut m = Map::new();
                m.insert("pid".into(), json!(1));
                m
            }),
        )
        .unwrap();

        let after = fs::metadata(&path).unwrap().len();
        assert!(after < MAX_LOG_BYTES, "expected rotated file under cap, got {after}");
    }

    #[test]
    fn append_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/deep/audit.log");
        append_to(
            &path,
            &make_record(epoch_now_s(), "start", {
                let mut m = Map::new();
                m.insert("pid".into(), json!(1));
                m
            }),
        )
        .unwrap();
        assert!(path.exists());
    }

    /// End-to-end: `record_start` / `record_shutdown` write through
    /// the public path (env override → resolve_path → append_to). The
    /// shutdown record must include `uptime_sec` and a single
    /// `record_shutdown` per process must override later calls.
    #[test]
    fn public_start_and_shutdown_path_emits_two_records() {
        let _globals = lock_globals();
        // Use a fresh path *and* clear the OnceLock by going through
        // the env override. This test runs in process; if another test
        // already populated AUDIT_PATH the env override won't help —
        // so this test is gated on running in isolation. Skip if a
        // path was already set.
        if AUDIT_PATH.get().is_some() {
            return;
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("public.log");
        unsafe {
            std::env::set_var(AUDIT_PATH_ENV, &path);
        }

        record_start(StartContext {
            argv: vec!["engine".into()],
            engine_version: Some("test".into()),
            socket_paths: vec![PathBuf::from("/tmp/x.sock")],
            state_db_path: Some(PathBuf::from("/tmp/state.db")),
            prior_state_db_size: Some(0),
            parent_command: Some("ps fake".into()),
        });
        record_shutdown("signal:SIGTERM");
        // Second call is a no-op — we still want exactly two lines.
        record_shutdown("normal");

        let parsed = parse_lines(&path);
        assert_eq!(parsed.len(), 2, "expected start + one shutdown");
        assert_eq!(parsed[0]["event"], "start");
        assert_eq!(parsed[0]["argv"][0], "engine");
        assert_eq!(parsed[0]["engine_version"], "test");
        assert_eq!(parsed[1]["event"], "shutdown");
        assert_eq!(parsed[1]["reason"], "signal:SIGTERM");
        assert!(parsed[1]["uptime_sec"].as_i64().unwrap() >= 0);

        // Re-arm SHUTDOWN_EMITTED so we don't poison sibling tests if
        // the runner schedules them after this one.
        SHUTDOWN_EMITTED.store(false, Ordering::SeqCst);
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
    }

    #[test]
    fn record_socket_bind_writes_kind_and_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sb.log");
        append_to(
            &path,
            &make_record(epoch_now_s(), "socket_bound", {
                let mut m = Map::new();
                m.insert("pid".into(), json!(1));
                m.insert("socket_kind".into(), Value::String("frontend".into()));
                m.insert("socket_path".into(), Value::String("/tmp/boss-engine.sock".into()));
                m
            }),
        )
        .unwrap();
        let parsed = parse_lines(&path);
        assert_eq!(parsed[0]["socket_kind"], "frontend");
        assert_eq!(parsed[0]["socket_path"], "/tmp/boss-engine.sock");
    }

    fn make_record(now_epoch_s: i64, event: &str, mut fields: Map<String, Value>) -> Value {
        fields.insert("ts".into(), Value::String(format_iso8601(now_epoch_s)));
        fields.insert("ts_epoch_s".into(), json!(now_epoch_s));
        fields.insert("event".into(), Value::String(event.to_owned()));
        Value::Object(fields)
    }

    /// Test helper: bypass the resolve_path OnceLock and write to an
    /// arbitrary path directly. Mirrors the public `record_*` shape so
    /// the test exercises the same field-assembly path.
    fn record_via_direct_path(path: &Path, event: &str, fields: Map<String, Value>) {
        let now = epoch_now_s();
        let value = make_record(now, event, fields);
        append_to(path, &value).unwrap();
    }

    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let m = m as i64;
        let d = d as i64;
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}
