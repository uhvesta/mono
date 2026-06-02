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

use boss_log_files::{next_rotated_path, rotated_segments};

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
///
/// [`rotated_segments`] returns the `<base>.<unix_seconds>` files oldest-first
/// (ascending timestamp), so the oldest `len - max_files` are simply the
/// leading slice. The rotated-segment format and ordering both live in
/// `boss-log-files` — this writer never re-encodes them.
pub fn prune_old_rotated(active_path: &Path, max_files: usize) {
    let backups = rotated_segments(active_path);
    if backups.len() <= max_files {
        return;
    }
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
        let backups = rotated_segments(&path);
        assert_eq!(backups.len(), 1, "expected one rotated backup");
    }

    #[test]
    fn rotate_on_startup_noop_when_file_absent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        rotate_on_startup(&path, 5);
        assert!(rotated_segments(&path).is_empty());
    }

    #[test]
    fn prune_keeps_n_most_recent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // Create 8 fake rotated files with ascending timestamps.
        for i in 1_000_u64..=1_007 {
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"data").unwrap();
        }

        prune_old_rotated(&path, 5);

        let backups = rotated_segments(&path);
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
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"data").unwrap();
        }
        prune_old_rotated(&path, 5);
        assert_eq!(rotated_segments(&path).len(), 3);
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

        let backups = rotated_segments(&path);
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
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"old").unwrap();
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

        let backups = rotated_segments(&path);
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

        assert!(rotated_segments(&path).is_empty(), "no rotation expected below threshold");
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
}
