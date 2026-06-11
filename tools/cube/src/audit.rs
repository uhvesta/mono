use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::app::CubeError;
use crate::paths;

const FILE_PREFIX: &str = "audit-";
const FILE_SUFFIX: &str = ".log";
const RETENTION_WEEKS: i64 = 12;
const SECS_PER_DAY: i64 = 86_400;

/// Append a structured event to cube's audit log.
///
/// ```ignore
/// audit!(database_path, "lease.acquired",
///     repo = workspace.repo,
///     workspace_id = workspace.workspace_id,
///     lease_id = lease_id,
///     keep_dirty = false,
///     reason = reason.as_deref(), // Option fields are dropped when None
/// );
/// ```
///
/// Each value can be any `Serialize` type. Field expressions are
/// borrowed, so values are not consumed.
#[macro_export]
macro_rules! audit {
    ($db_path:expr, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        let mut __fields = ::serde_json::Map::new();
        $(
            $crate::audit::insert_field(&mut __fields, stringify!($key), &($value));
        )*
        $crate::audit::record($db_path, $event, __fields);
    }};
}

/// Best-effort audit log emission. Used by the [`audit!`] macro; not
/// usually called directly. Failures are logged to stderr — a failed
/// audit must never block a lease or release.
///
/// `database_path` is threaded through to keep tests self-contained:
/// when a test passes a tempdir database, the audit log lives next to
/// it. In production callers pass `None` and the audit dir falls back
/// to the standard data dir.
pub fn record(database_path: Option<&Path>, event: &str, fields: Map<String, Value>) {
    let dir = match audit_dir(database_path) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("warning: cube audit log location unavailable: {err}");
            return;
        }
    };
    if let Err(err) = append(&dir, event, fields) {
        eprintln!("warning: failed to append cube audit log: {err}");
    }
}

/// Insert one field into an audit-event payload. Anything that
/// implements `Serialize` works (`String`, `&str`, numbers, bools,
/// `Option<T>`, …). `Option::None` and any value that serializes to
/// JSON `null` is dropped so the resulting line stays tidy.
///
/// Used by the [`audit!`] macro; rarely called directly.
pub fn insert_field<T>(fields: &mut Map<String, Value>, key: &str, value: &T)
where
    T: Serialize + ?Sized,
{
    match serde_json::to_value(value) {
        Ok(Value::Null) => {}
        Ok(v) => {
            fields.insert(key.to_string(), v);
        }
        Err(err) => {
            eprintln!("warning: cube audit field `{key}` failed to serialize: {err}");
        }
    }
}

fn audit_dir(database_path: Option<&Path>) -> Result<PathBuf, CubeError> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(paths::audit_dir_in(parent)),
        None => paths::audit_dir(),
    }
}

fn append(dir: &Path, event: &str, fields: Map<String, Value>) -> Result<(), CubeError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CubeError::Io(std::io::Error::other(e)))?
        .as_secs() as i64;
    append_at(dir, now, event, fields)
}

pub(crate) fn append_at(
    dir: &Path,
    now_epoch_s: i64,
    event: &str,
    mut fields: Map<String, Value>,
) -> Result<(), CubeError> {
    std::fs::create_dir_all(dir).map_err(|e| CubeError::AuditLogIo {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let path = dir.join(week_file_name(now_epoch_s));

    fields.insert("ts".into(), Value::String(format_iso8601(now_epoch_s)));
    fields.insert("ts_epoch_s".into(), Value::Number(now_epoch_s.into()));
    fields.insert("event".into(), Value::String(event.to_string()));

    let mut line = serde_json::to_string(&Value::Object(fields))?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| CubeError::AuditLogIo {
            path: path.clone(),
            source: e,
        })?;
    file.write_all(line.as_bytes()).map_err(|e| CubeError::AuditLogIo {
        path: path.clone(),
        source: e,
    })?;

    let _ = prune(dir, now_epoch_s);
    Ok(())
}

fn week_file_name(now_epoch_s: i64) -> String {
    let (y, m, d) = monday_of_week(now_epoch_s);
    format!("{FILE_PREFIX}{y:04}-{m:02}-{d:02}{FILE_SUFFIX}")
}

fn prune(dir: &Path, now_epoch_s: i64) -> std::io::Result<()> {
    let cutoff = now_epoch_s - RETENTION_WEEKS * 7 * SECS_PER_DAY;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(date_str) = name.strip_prefix(FILE_PREFIX).and_then(|s| s.strip_suffix(FILE_SUFFIX)) else {
            continue;
        };
        if let Some(file_epoch) = parse_yyyy_mm_dd(date_str)
            && file_epoch < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn parse_yyyy_mm_dd(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * SECS_PER_DAY)
}

fn monday_of_week(epoch_s: i64) -> (i64, u32, u32) {
    let day = epoch_s.div_euclid(SECS_PER_DAY);
    // 1970-01-01 was a Thursday. With Monday = 0..Sunday = 6, Thursday = 3.
    let weekday = (day + 3).rem_euclid(7);
    civil_from_days(day - weekday)
}

// Howard Hinnant's date algorithms — see
// https://howardhinnant.github.io/date_algorithms.html
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

fn format_iso8601(epoch_s: i64) -> String {
    let day = epoch_s.div_euclid(SECS_PER_DAY);
    let sec_in_day = epoch_s.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(day);
    let h = sec_in_day / 3600;
    let mi = (sec_in_day / 60) % 60;
    let s = sec_in_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn fields() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("repo".into(), json!("mono"));
        m.insert("workspace_id".into(), json!("mono-agent-004"));
        m
    }

    fn epoch_for(y: i64, m: u32, d: u32, hh: i64, mm: i64, ss: i64) -> i64 {
        days_from_civil(y, m, d) * SECS_PER_DAY + hh * 3600 + mm * 60 + ss
    }

    #[test]
    fn week_file_name_uses_monday_of_iso_week() {
        // 2026-04-30 (Thursday) at 14:05:30 UTC.
        let ts = epoch_for(2026, 4, 30, 14, 5, 30);
        assert_eq!(week_file_name(ts), "audit-2026-04-27.log");

        // 2026-04-27 (Monday, same week) — same file.
        let ts_mon = epoch_for(2026, 4, 27, 0, 0, 0);
        assert_eq!(week_file_name(ts_mon), "audit-2026-04-27.log");

        // 2026-05-03 (Sunday) — still that week's Monday.
        let ts_sun = epoch_for(2026, 5, 3, 23, 59, 59);
        assert_eq!(week_file_name(ts_sun), "audit-2026-04-27.log");

        // 2026-05-04 (next Monday) rolls to a new file.
        let ts_next = epoch_for(2026, 5, 4, 0, 0, 1);
        assert_eq!(week_file_name(ts_next), "audit-2026-05-04.log");
    }

    #[test]
    fn iso8601_round_trip_for_known_instant() {
        // 2026-04-30 14:05:30 UTC.
        let ts = epoch_for(2026, 4, 30, 14, 5, 30);
        assert_eq!(format_iso8601(ts), "2026-04-30T14:05:30Z");
    }

    #[test]
    fn append_writes_jsonl_with_event_and_timestamp() {
        let dir = tempdir().unwrap();
        let ts = epoch_for(2026, 4, 30, 14, 5, 30);
        append_at(dir.path(), ts, "lease.acquired", fields()).unwrap();

        let contents = fs::read_to_string(dir.path().join("audit-2026-04-27.log")).expect("audit file");
        let parsed: Value = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(parsed["event"], "lease.acquired");
        assert_eq!(parsed["ts"], "2026-04-30T14:05:30Z");
        assert_eq!(parsed["ts_epoch_s"], ts);
        assert_eq!(parsed["repo"], "mono");
        assert_eq!(parsed["workspace_id"], "mono-agent-004");
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn appending_in_same_week_reuses_file() {
        let dir = tempdir().unwrap();
        let mon = epoch_for(2026, 4, 27, 9, 0, 0);
        let fri = epoch_for(2026, 5, 1, 17, 30, 0);
        append_at(dir.path(), mon, "lease.acquired", fields()).unwrap();
        append_at(dir.path(), fri, "lease.released", fields()).unwrap();

        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1);
        let lines = fs::read_to_string(dir.path().join("audit-2026-04-27.log")).unwrap();
        assert_eq!(lines.lines().count(), 2);
    }

    #[test]
    fn weekly_rollover_creates_new_file() {
        let dir = tempdir().unwrap();
        let week_a = epoch_for(2026, 4, 30, 12, 0, 0);
        let week_b = epoch_for(2026, 5, 7, 12, 0, 0);
        append_at(dir.path(), week_a, "lease.acquired", fields()).unwrap();
        append_at(dir.path(), week_b, "lease.acquired", fields()).unwrap();

        assert!(dir.path().join("audit-2026-04-27.log").exists());
        assert!(dir.path().join("audit-2026-05-04.log").exists());
    }

    #[test]
    fn prune_removes_files_older_than_retention_window() {
        let dir = tempdir().unwrap();
        // Write a fake old file dated 20 weeks ago.
        let old = dir.path().join("audit-2025-12-01.log");
        fs::write(&old, "{}\n").unwrap();
        // And one inside the retention window (4 weeks ago).
        let recent = dir.path().join("audit-2026-04-06.log");
        fs::write(&recent, "{}\n").unwrap();

        let now = epoch_for(2026, 4, 30, 12, 0, 0);
        append_at(dir.path(), now, "lease.acquired", fields()).unwrap();

        assert!(!old.exists(), "expected old log to be pruned");
        assert!(recent.exists(), "expected recent log to be kept");
        assert!(dir.path().join("audit-2026-04-27.log").exists());
    }

    #[test]
    fn prune_ignores_unrelated_files() {
        let dir = tempdir().unwrap();
        let foreign = dir.path().join("README");
        fs::write(&foreign, "hello").unwrap();
        let now = epoch_for(2026, 4, 30, 12, 0, 0);
        append_at(dir.path(), now, "lease.acquired", fields()).unwrap();
        assert!(foreign.exists());
    }
}
