//! Periodic and startup backup of the engine's state database.
//!
//! Takes a consistent snapshot using SQLite's `VACUUM INTO` command, which
//! checkpoints the WAL into a clean copy without requiring an exclusive
//! lock — concurrent reads and writes continue while the snapshot is in
//! progress. Backups are named `state.db.bak-YYYYMMDD-HHMMSS` (UTC) and
//! stored in a configurable backup directory. A retention policy deletes
//! old backups beyond a configurable count so disk use stays bounded.
//!
//! ## Configuration via environment variables
//!
//! All settings have safe defaults; no configuration is required for
//! correct behaviour:
//!
//! | Variable | Default | Purpose |
//! |---|---|---|
//! | `BOSS_BACKUP_DIR` | `<state_root>/backups` | Backup directory |
//! | `BOSS_BACKUP_INTERVAL_SECS` | `3600` | Seconds between backups |
//! | `BOSS_BACKUP_RETENTION` | `24` | Maximum backups to keep |
//!
//! ## Non-fatal by construction
//!
//! Like [`crate::recovery_backup`], every failure mode is logged and
//! swallowed. A backup failure never propagates to the engine's request
//! path. In-memory databases (used by the test suite) are silently skipped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::work::WorkDb;

/// Env override for the backup directory.
pub const BACKUP_DIR_ENV: &str = "BOSS_BACKUP_DIR";
/// Env override for the backup interval in seconds.
pub const BACKUP_INTERVAL_SECS_ENV: &str = "BOSS_BACKUP_INTERVAL_SECS";
/// Env override for the maximum number of backups to retain.
pub const BACKUP_RETENTION_ENV: &str = "BOSS_BACKUP_RETENTION";

/// Default backup interval: one hour.
pub const DEFAULT_BACKUP_INTERVAL: Duration = Duration::from_secs(3600);
/// Default retention: 24 most-recent backups.
pub const DEFAULT_RETENTION_COUNT: usize = 24;

const BACKUP_FILE_PREFIX: &str = "state.db.bak-";

/// Resolve the backup directory: `BOSS_BACKUP_DIR` override wins, then
/// `<state_root>/backups`.
pub fn default_backup_dir(state_root: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os(BACKUP_DIR_ENV) {
        return PathBuf::from(dir);
    }
    state_root.join("backups")
}

/// Read the backup interval from `BOSS_BACKUP_INTERVAL_SECS`, or fall
/// back to [`DEFAULT_BACKUP_INTERVAL`].
pub fn backup_interval() -> Duration {
    std::env::var(BACKUP_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_BACKUP_INTERVAL)
}

/// Read the retention count from `BOSS_BACKUP_RETENTION`, or fall back
/// to [`DEFAULT_RETENTION_COUNT`].
pub fn retention_count() -> usize {
    std::env::var(BACKUP_RETENTION_ENV)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_RETENTION_COUNT)
}

/// Take a snapshot of `work_db` to `dest` using SQLite's `VACUUM INTO`.
///
/// Creates `dest`'s parent directory if it does not exist. `dest` must
/// not already exist — pass a freshly-timestamped path on every call.
/// Fails immediately if `work_db` is an in-memory database.
pub fn take_backup(work_db: &WorkDb, dest: &Path) -> Result<()> {
    if work_db.is_in_memory() {
        anyhow::bail!("cannot VACUUM INTO an in-memory database");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create backup dir {}", parent.display()))?;
    }
    let dest_str = dest
        .to_str()
        .with_context(|| format!("backup path is not valid UTF-8: {}", dest.display()))?;
    // SQL string literal: escape embedded single quotes as ''.
    let escaped = dest_str.replace('\'', "''");
    let conn = work_db.connect()?;
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .with_context(|| format!("VACUUM INTO {}", dest.display()))?;
    Ok(())
}

/// Delete the oldest `state.db.bak-*` files in `backup_dir`, keeping at
/// most `keep`. Files that do not match the backup prefix are untouched.
pub fn apply_retention(backup_dir: &Path, keep: usize) -> Result<()> {
    let read_dir =
        std::fs::read_dir(backup_dir).with_context(|| format!("read backup dir {}", backup_dir.display()))?;

    let mut backups: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(BACKUP_FILE_PREFIX))
                .unwrap_or(false)
        })
        .collect();

    // `YYYYMMDD-HHMMSS` names sort lexicographically oldest-first.
    backups.sort();

    let to_delete = backups.len().saturating_sub(keep);
    for path in backups.iter().take(to_delete) {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::debug!(
                path = %path.display(),
                "database-backup: deleted old backup",
            ),
            Err(err) => tracing::warn!(
                path = %path.display(),
                error = %err,
                "database-backup: could not delete old backup (non-fatal)",
            ),
        }
    }
    Ok(())
}

/// Format the current UTC time as `YYYYMMDD-HHMMSS`.
///
/// `chrono`'s `clock` feature is disabled in this workspace (the
/// scheduler injects "now" as epoch seconds). We bridge the gap by
/// reading the system clock via [`std::time::SystemTime`] and converting
/// the resulting epoch seconds to a `DateTime<Utc>` via
/// [`chrono::TimeZone::timestamp_opt`], which does not need the clock
/// feature.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// Best-effort backup entry point: snapshot then retention.
///
/// In-memory databases are silently skipped. All other failures are
/// logged at `warn` and swallowed so the caller's main flow is unaffected.
pub fn run_backup(work_db: &WorkDb, backup_dir: &Path, retention: usize) {
    if work_db.is_in_memory() {
        return;
    }
    let ts = utc_timestamp();
    let dest = backup_dir.join(format!("{BACKUP_FILE_PREFIX}{ts}"));
    match take_backup(work_db, &dest) {
        Ok(()) => tracing::info!(
            path = %dest.display(),
            "database-backup: snapshot complete",
        ),
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "database-backup: snapshot failed (non-fatal)",
            );
            return;
        }
    }
    if let Err(err) = apply_retention(backup_dir, retention) {
        tracing::warn!(
            error = %format!("{err:#}"),
            "database-backup: retention enforcement failed (non-fatal)",
        );
    }
}

/// Spawn a background task that calls [`run_backup`] immediately on boot
/// and then once per `interval`.
///
/// Fires immediately so the engine always has a fresh backup at startup.
/// Uses `spawn_blocking` because `VACUUM INTO` is a synchronous I/O
/// operation that would otherwise stall the async runtime.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    backup_dir: PathBuf,
    interval: Duration,
    retention: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let db = work_db.clone();
            let dir = backup_dir.clone();
            tokio::task::spawn_blocking(move || run_backup(db.as_ref(), &dir, retention))
                .await
                .ok();
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_file_db(dir: &Path) -> WorkDb {
        WorkDb::open(dir.join("state.db")).expect("open test db")
    }

    // ── take_backup ───────────────────────────────────────────────

    #[test]
    fn backup_creates_file_with_content() {
        let tmp = TempDir::new().unwrap();
        let db = open_file_db(tmp.path());
        let dest = tmp.path().join("backups").join("state.db.bak-test");
        take_backup(&db, &dest).expect("backup should succeed");
        assert!(dest.exists(), "backup file must be created");
        assert!(dest.metadata().unwrap().len() > 0, "backup file must be non-empty");
    }

    #[test]
    fn backup_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let db = open_file_db(tmp.path());
        let dest = tmp.path().join("nested").join("deep").join("state.db.bak-test");
        take_backup(&db, &dest).expect("backup should create missing dirs");
        assert!(dest.exists());
    }

    #[test]
    fn backup_fails_for_in_memory_db() {
        // WorkDb::open(":memory:") routes to an in-memory database.
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let tmp = TempDir::new().unwrap();
        let result = take_backup(&db, &tmp.path().join("out.db"));
        assert!(result.is_err(), "in-memory backup must fail with an error");
    }

    // ── apply_retention ───────────────────────────────────────────

    #[test]
    fn retention_deletes_oldest_backups() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        for i in 1..=5u8 {
            std::fs::write(dir.join(format!("state.db.bak-2026010{i}-120000")), b"x").unwrap();
        }
        apply_retention(dir, 3).expect("retention should succeed");
        let remaining: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(remaining.len(), 3, "should keep only the 3 newest");
        assert!(!dir.join("state.db.bak-20260101-120000").exists());
        assert!(!dir.join("state.db.bak-20260102-120000").exists());
        assert!(dir.join("state.db.bak-20260103-120000").exists());
    }

    #[test]
    fn retention_ignores_non_backup_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("state.db.bak-20260101-120000"), b"x").unwrap();
        std::fs::write(dir.join("state.db.bak-20260102-120000"), b"x").unwrap();
        std::fs::write(dir.join("other.txt"), b"x").unwrap();
        apply_retention(dir, 1).expect("retention should succeed");
        assert!(
            !dir.join("state.db.bak-20260101-120000").exists(),
            "older backup must be removed"
        );
        assert!(
            dir.join("state.db.bak-20260102-120000").exists(),
            "newest backup must be kept"
        );
        assert!(dir.join("other.txt").exists(), "non-backup file must be kept");
    }

    #[test]
    fn retention_noop_when_under_limit() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("state.db.bak-20260101-120000"), b"x").unwrap();
        apply_retention(dir, 10).expect("retention should succeed");
        assert!(dir.join("state.db.bak-20260101-120000").exists());
    }

    // ── run_backup ────────────────────────────────────────────────

    #[test]
    fn run_backup_creates_backup_and_enforces_retention() {
        let tmp = TempDir::new().unwrap();
        let db = open_file_db(tmp.path());
        let backup_dir = tmp.path().join("backups");
        // Pre-populate with more files than the keep limit.
        std::fs::create_dir_all(&backup_dir).unwrap();
        for i in 1..=3u8 {
            std::fs::write(backup_dir.join(format!("state.db.bak-2020010{i}-000000")), b"old").unwrap();
        }
        // run_backup with retention=2 should add one new file and delete the
        // oldest two so only 2 remain (the new one + one pre-existing).
        run_backup(&db, &backup_dir, 2);
        let remaining: Vec<_> = std::fs::read_dir(&backup_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(
            remaining.len(),
            2,
            "retention must leave exactly 2 backups; got {remaining:?}"
        );
    }

    #[test]
    fn run_backup_skips_in_memory_db() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let tmp = TempDir::new().unwrap();
        run_backup(&db, tmp.path(), 24);
        // No backup file should have been created.
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().filter_map(|e| e.ok()).collect();
        assert!(
            entries.is_empty(),
            "run_backup must not create files for in-memory databases"
        );
    }

    // ── default_backup_dir ────────────────────────────────────────

    #[test]
    fn default_backup_dir_env_override() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(BACKUP_DIR_ENV);
        unsafe { std::env::set_var(BACKUP_DIR_ENV, "/tmp/boss-test-backups") };
        let dir = default_backup_dir(Path::new("/state/root"));
        match prev {
            Some(v) => unsafe { std::env::set_var(BACKUP_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(BACKUP_DIR_ENV) },
        }
        assert_eq!(dir, PathBuf::from("/tmp/boss-test-backups"));
    }

    #[test]
    fn default_backup_dir_falls_back_to_state_root() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(BACKUP_DIR_ENV);
        unsafe { std::env::remove_var(BACKUP_DIR_ENV) };
        let dir = default_backup_dir(Path::new("/state/root"));
        match prev {
            Some(v) => unsafe { std::env::set_var(BACKUP_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(BACKUP_DIR_ENV) },
        }
        assert_eq!(dir, PathBuf::from("/state/root/backups"));
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }
}
