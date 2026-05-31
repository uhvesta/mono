//! Rotation and retention for `engine-trace.jsonl`.
//!
//! On engine startup the existing trace file (if any) is renamed to a
//! timestamped backup (`engine-trace.jsonl.<unix_s>`).  During the run,
//! [`RotatingJsonlWriter`] checks the running byte count after every
//! write and rotates when the threshold is crossed.
//!
//! Rotated files are pruned to the N most recent; older files are
//! deleted automatically.
//!
//! ## Configuration (env overrides; defaults are safe without any config)
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `BOSS_ENGINE_TRACE_MAX_BYTES` | `104857600` (100 MiB) | Rotate when file exceeds this size |
//! | `BOSS_ENGINE_TRACE_MAX_FILES` | `5` | Keep at most this many rotated backups |
//!
//! ## Rotation safety
//!
//! Rotation happens while the writer's mutex is held, so no concurrent
//! write can race with the rename + re-open.  On Unix, an open file
//! descriptor follows the inode through a rename, so any unflushed bytes
//! already written land safely in the renamed file before the old `File`
//! handle is dropped.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TRACE_MAX_BYTES_ENV: &str = "BOSS_ENGINE_TRACE_MAX_BYTES";
pub const TRACE_MAX_FILES_ENV: &str = "BOSS_ENGINE_TRACE_MAX_FILES";

/// Default maximum size before rotation: 100 MiB.
pub const DEFAULT_TRACE_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Default number of rotated backups to keep.
pub const DEFAULT_TRACE_MAX_FILES: usize = 5;

/// Read rotation config from env vars, falling back to safe defaults.
pub fn trace_rotation_config() -> (u64, usize) {
    let max_bytes = std::env::var(TRACE_MAX_BYTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TRACE_MAX_BYTES);
    let max_files = std::env::var(TRACE_MAX_FILES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TRACE_MAX_FILES);
    (max_bytes, max_files)
}

/// Called once at engine startup before opening the trace file.
///
/// If `path` already exists, renames it to a timestamped backup and
/// then prunes old backups so only `max_files` remain.  Any error is
/// printed to stderr and swallowed — trace rotation must never block
/// engine startup.
pub fn rotate_on_startup(path: &Path, max_files: usize) {
    if !path.exists() {
        return;
    }
    let rotated = next_rotated_path(path);
    if let Err(err) = std::fs::rename(path, &rotated) {
        eprintln!("boss-engine: could not rotate engine-trace.jsonl on startup: {err}");
        return;
    }
    prune_old_rotated(path, max_files);
}

/// Open (or create) the trace file for appending.  The directory is
/// created if needed.  Called both at startup and after each rotation.
pub fn open_trace_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Delete the oldest rotated backups, keeping at most `max_files`.
/// Silently ignores any deletion error — this is best-effort cleanup.
pub fn prune_old_rotated(active_path: &Path, max_files: usize) {
    let mut backups = list_rotated_files(active_path);
    if backups.len() <= max_files {
        return;
    }
    // Sort ascending by name.  The suffix is a 10-digit Unix timestamp,
    // so lexicographic order equals chronological order.
    backups.sort();
    let to_delete = backups.len() - max_files;
    for path in &backups[..to_delete] {
        if let Err(err) = std::fs::remove_file(path) {
            eprintln!(
                "boss-engine: could not prune old trace file {}: {err}",
                path.display()
            );
        }
    }
}

/// Returns a non-existing rotated path using a Unix-second timestamp
/// suffix.  Increments the timestamp by one until a free slot is found,
/// handling the rare case of multiple rotations within the same second.
fn next_rotated_path(path: &Path) -> PathBuf {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut ts = base;
    loop {
        let candidate = rotated_path_at(path, ts);
        if !candidate.exists() {
            return candidate;
        }
        ts += 1;
    }
}

/// Build the rotated file path by replacing the `.jsonl` extension with
/// `.jsonl.<ts_secs>` (e.g. `engine-trace.jsonl.1748694000`).
fn rotated_path_at(path: &Path, ts_secs: u64) -> PathBuf {
    path.with_extension(format!("jsonl.{ts_secs}"))
}

/// List all rotated backups in the same directory as `active_path`.
/// A file qualifies iff its name is `<active_filename>.<all-digits>`.
fn list_rotated_files(active_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = active_path.parent() else {
        return vec![];
    };
    let Some(stem) = active_path.file_name().and_then(|n| n.to_str()) else {
        return vec![];
    };
    let prefix = format!("{stem}.");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let suffix = n.strip_prefix(prefix.as_str()).unwrap_or("");
                    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
                })
                .unwrap_or(false)
        })
        .collect()
}

/// Mutable state held inside the writer's mutex — the current open file
/// and how many bytes have been written to it since the last rotation.
pub struct RotatingState {
    pub file: File,
    pub bytes_written: u64,
}

impl RotatingState {
    /// Create state from an already-open file.  Reads the current file
    /// size so the byte counter is accurate even if the file existed
    /// before startup rotation ran (e.g. rotation was skipped on error).
    pub fn new(file: File) -> Self {
        let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Self { file, bytes_written }
    }
}

/// `Write` impl for `engine-trace.jsonl` that rotates the file when the
/// byte threshold is crossed.
///
/// The `Arc<Mutex<Option<RotatingState>>>` is cloned cheaply by the
/// `move || RotatingJsonlWriter { … }` closure on each log event, so
/// all instances share the same underlying state and byte counter.
/// When `state` is `None` (file could not be opened) every write is a
/// silent no-op, matching the original `JsonlFileWriter` behaviour.
pub struct RotatingJsonlWriter {
    pub path: PathBuf,
    pub state: Arc<Mutex<Option<RotatingState>>>,
    pub max_bytes: u64,
    pub max_files: usize,
}

impl Write for RotatingJsonlWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Ok(mut guard) = self.state.lock() else {
            return Ok(buf.len()); // poisoned mutex — no-op
        };
        let Some(state) = guard.as_mut() else {
            return Ok(buf.len()); // no file — no-op
        };

        let _ = state.file.write_all(buf);
        state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);

        if state.bytes_written >= self.max_bytes {
            // Rotate inline while holding the lock so no concurrent
            // write can race with the rename + re-open sequence.
            let rotated = next_rotated_path(&self.path);
            if std::fs::rename(&self.path, &rotated).is_ok() {
                match open_trace_file(&self.path) {
                    Ok(new_file) => {
                        state.file = new_file;
                        state.bytes_written = 0;
                        prune_old_rotated(&self.path, self.max_files);
                    }
                    Err(err) => {
                        eprintln!(
                            "boss-engine: could not open new trace file after rotation: {err}"
                        );
                    }
                }
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let Ok(mut guard) = self.state.lock() else {
            return Ok(());
        };
        if let Some(state) = guard.as_mut() {
            let _ = state.file.flush();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn tmp_trace(dir: &TempDir) -> PathBuf {
        dir.path().join("engine-trace.jsonl")
    }

    #[test]
    fn rotate_on_startup_renames_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        fs::write(&path, b"line1\n").unwrap();

        rotate_on_startup(&path, 5);

        assert!(!path.exists(), "active path should be gone after startup rotation");
        let backups = list_rotated_files(&path);
        assert_eq!(backups.len(), 1, "expected one rotated backup");
    }

    #[test]
    fn rotate_on_startup_noop_when_file_absent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        rotate_on_startup(&path, 5);
        assert!(list_rotated_files(&path).is_empty());
    }

    #[test]
    fn prune_keeps_n_most_recent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // Create 8 fake rotated files with ascending timestamps.
        for i in 1_000_u64..=1_007 {
            fs::write(rotated_path_at(&path, i), b"data").unwrap();
        }

        prune_old_rotated(&path, 5);

        let backups = list_rotated_files(&path);
        assert_eq!(backups.len(), 5, "expected 5 survivors");
        // The 5 newest (highest timestamp) must survive.
        let mut names: Vec<_> = backups
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for (i, name) in names.iter().enumerate() {
            let expected_ts = 1003 + i as u64;
            assert!(
                name.ends_with(&expected_ts.to_string()),
                "expected ts {expected_ts} in {name}"
            );
        }
    }

    #[test]
    fn prune_noop_when_within_limit() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        for i in 0_u64..3 {
            fs::write(rotated_path_at(&path, i), b"data").unwrap();
        }
        prune_old_rotated(&path, 5);
        assert_eq!(list_rotated_files(&path).len(), 3);
    }

    #[test]
    fn rotating_writer_rotates_on_threshold() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = RotatingJsonlWriter {
            path: path.clone(),
            state: state.clone(),
            max_bytes: 10,
            max_files: 3,
        };

        // 15 bytes exceeds the 10-byte threshold.
        writer.write_all(b"123456789012345").unwrap();

        let backups = list_rotated_files(&path);
        assert_eq!(backups.len(), 1, "expected one rotated backup after write");
        assert!(path.exists(), "new active file should exist after rotation");
        let guard = state.lock().unwrap();
        let s = guard.as_ref().unwrap();
        assert_eq!(s.bytes_written, 0, "byte counter should reset after rotation");
    }

    #[test]
    fn rotating_writer_prunes_beyond_max_files() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // Pre-populate 3 old rotated files.
        for i in 1_000_u64..=1_002 {
            fs::write(rotated_path_at(&path, i), b"old").unwrap();
        }
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = RotatingJsonlWriter {
            path: path.clone(),
            state,
            max_bytes: 5,
            max_files: 2,
        };

        // 6 bytes exceeds the 5-byte threshold → rotation + prune.
        writer.write_all(b"123456").unwrap();

        let backups = list_rotated_files(&path);
        assert!(
            backups.len() <= 2,
            "expected at most 2 rotated backups after prune, got {}",
            backups.len()
        );
    }

    #[test]
    fn no_rotation_below_threshold() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = RotatingJsonlWriter {
            path: path.clone(),
            state,
            max_bytes: 100,
            max_files: 3,
        };

        writer.write_all(b"small").unwrap();

        assert!(list_rotated_files(&path).is_empty(), "no rotation expected below threshold");
    }

    #[test]
    fn noop_writer_when_state_is_none() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let state: Arc<Mutex<Option<RotatingState>>> = Arc::new(Mutex::new(None));
        let mut writer = RotatingJsonlWriter {
            path: path.clone(),
            state,
            max_bytes: 10,
            max_files: 3,
        };
        // Should not panic or create any file.
        let n = writer.write(b"data").unwrap();
        assert_eq!(n, 4);
        assert!(!path.exists());
    }

    #[test]
    fn list_rotated_files_ignores_non_numeric_suffixes() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // A file with a non-numeric suffix should not be listed.
        fs::write(
            dir.path().join("engine-trace.jsonl.old"),
            b"x",
        )
        .unwrap();
        // A valid rotated file.
        fs::write(rotated_path_at(&path, 9999), b"x").unwrap();
        let backups = list_rotated_files(&path);
        assert_eq!(backups.len(), 1);
        assert!(
            backups[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".9999")
        );
    }
}
