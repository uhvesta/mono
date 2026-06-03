//! Auto-backup of a dead execution's uncommitted workspace work to a
//! durable patch file.
//!
//! When the engine detects that an execution has died unexpectedly (a
//! killed worker process, a crashed/force-quit UI, a host restart) the
//! cube workspace it was leased into may still hold in-flight,
//! uncommitted work that was never pushed to origin. Today that work
//! survives only as long as the workspace's dirty working copy survives
//! — it is lost the moment the workspace is re-leased and reset.
//!
//! This module captures that work *proactively and automatically* at
//! death-detection time, so a recoverable artifact always exists
//! independent of whether the workspace can later be reclaimed. The
//! capture is a one-shot `jj diff --git` of the workspace's working copy
//! (equivalent to `jj diff --git -R <ws>`), written to a stable,
//! engine-owned recovery location keyed by execution id
//! (`<recovery_dir>/<exec-id>.patch`).
//!
//! ## Where the at-death hooks live
//!
//! [`backup_dead_execution`] is the single entry point. It is invoked
//! from every path that transitions a live execution to `orphaned`:
//!
//! - [`crate::dead_pid_sweep`] — worker PID probe reports the process
//!   is gone.
//! - [`crate::stale_worker_sweep`] — worker is wedged past the staleness
//!   threshold.
//! - the engine-startup reaper in [`crate::app`] — cube probe verdict
//!   `Dead` across an engine/UI restart.
//!
//! ## Why a patch and not a push
//!
//! Pushing on every worker turn is too heavy; a one-shot capture at
//! death is cheap and bounds the blast radius of a crash. The patch
//! also decouples recovery from workspace reclaim — even if cube hands
//! the resuming worker a fresh workspace, the patch is available to
//! replay the lost work via `git apply --3way`.
//!
//! ## Scope of the capture
//!
//! `jj diff` captures the working-copy commit (`@`) against its parent —
//! i.e. uncommitted work. Running `jj` itself snapshots the on-disk
//! state into `@` first, so the latest edits are included. Local commits
//! that the worker already `jj describe`d into ancestors of `@`, and
//! branches already pushed to origin, are out of scope: the former are a
//! possible future enhancement (capture `trunk..@`), the latter are
//! already durable on the remote.
//!
//! ## Non-fatal by construction
//!
//! Backup is a best-effort precaution layered on top of the reap. Every
//! failure mode (no recorded workspace path, missing directory, `jj`
//! unavailable or erroring, an empty diff) is logged and swallowed —
//! the caller's reap proceeds regardless. The function never returns an
//! error; on any failure it returns `None`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use boss_protocol::{ExecutionKind, WorkExecution};

/// Environment override for the recovery directory. Set by tests to
/// redirect captures into a tempdir; an operator can also point it at an
/// alternate location. When unset, [`default_recovery_dir`] falls back to
/// the engine's `Application Support` tree.
pub const RECOVERY_DIR_ENV: &str = "BOSS_RECOVERY_DIR";

/// Resolve the engine-owned recovery directory.
///
/// Honours [`RECOVERY_DIR_ENV`] first, then falls back to
/// `$HOME/Library/Application Support/Boss/recovery` (the same
/// `Application Support/Boss` tree that holds `state.db`). Returns `None`
/// only when neither the override nor `HOME` is set — in which case there
/// is nowhere durable to write and the caller skips the backup.
pub fn default_recovery_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(RECOVERY_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss/recovery"))
}

/// Map an execution id to a safe single-segment patch filename.
///
/// Execution ids are already `exec_<hex>_<n>`-shaped, but we defensively
/// replace anything outside `[A-Za-z0-9_-]` with `_` so a hostile or
/// malformed id can never escape the recovery directory via `/` or `..`.
fn patch_file_name(execution_id: &str) -> String {
    let stem: String = execution_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}.patch")
}

/// Capture `jj diff --git` for the workspace's working copy.
///
/// Runs with `current_dir` set to the workspace so it operates on that
/// secondary workspace's working copy exactly as the worker would —
/// equivalent to `jj diff --git -R <ws>`. Running `jj` snapshots the
/// on-disk working copy into `@` before diffing, so uncommitted edits are
/// included. Returns the raw git-format patch on stdout; errors if `jj`
/// cannot be spawned or exits non-zero.
fn capture_workspace_diff(workspace_path: &Path) -> Result<String> {
    let output = Command::new("jj")
        .args(["diff", "--git"])
        .current_dir(workspace_path)
        .output()
        .with_context(|| {
            format!(
                "failed to spawn `jj diff --git` in {}",
                workspace_path.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "`jj diff --git` exited with {} in {}: {}",
            output.status,
            workspace_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Write `diff` to `<recovery_dir>/<exec-id>.patch` when it is non-empty.
///
/// Creates `recovery_dir` if needed. An all-whitespace/empty diff means
/// the workspace had no uncommitted work — there is nothing to back up,
/// so no file is written and `Ok(None)` is returned. Otherwise the patch
/// is written and its path returned.
pub fn write_patch_if_nonempty(
    recovery_dir: &Path,
    execution_id: &str,
    diff: &str,
) -> Result<Option<PathBuf>> {
    if diff.trim().is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(recovery_dir).with_context(|| {
        format!(
            "failed to create recovery dir {}",
            recovery_dir.display()
        )
    })?;
    let path = recovery_dir.join(patch_file_name(execution_id));
    std::fs::write(&path, diff.as_bytes())
        .with_context(|| format!("failed to write recovery patch {}", path.display()))?;
    Ok(Some(path))
}

/// Capture the workspace's uncommitted diff and persist it as a patch.
///
/// Composition of [`capture_workspace_diff`] and
/// [`write_patch_if_nonempty`]; returns `Ok(None)` when the working copy
/// is clean. Exposed (rather than only [`backup_dead_execution`]) so the
/// capture can be exercised directly against a real jj workspace in
/// tests.
pub fn backup_execution_patch(
    recovery_dir: &Path,
    workspace_path: &Path,
    execution_id: &str,
) -> Result<Option<PathBuf>> {
    let diff = capture_workspace_diff(workspace_path)?;
    write_patch_if_nonempty(recovery_dir, execution_id, &diff)
}

/// Best-effort backup entry point for a dead execution.
///
/// Resolves the workspace path from the execution row and the recovery
/// dir from the environment, then captures and persists the patch. Every
/// failure mode is logged and swallowed — this is a precaution that must
/// never break the reap that triggered it — so the function returns the
/// patch path on success and `None` on any skip or failure.
pub fn backup_dead_execution(execution: &WorkExecution) -> Option<PathBuf> {
    let Some(workspace_path) = execution.workspace_path.as_deref() else {
        // No leased workspace recorded (e.g. the row never got past
        // pre-start). Nothing on disk to capture.
        return None;
    };
    let workspace_path = Path::new(workspace_path);
    if !workspace_path.is_dir() {
        tracing::warn!(
            execution_id = %execution.id,
            workspace_path = %workspace_path.display(),
            "recovery-backup: workspace path is not a directory; skipping patch capture",
        );
        return None;
    }
    let Some(recovery_dir) = default_recovery_dir() else {
        tracing::warn!(
            execution_id = %execution.id,
            "recovery-backup: cannot resolve recovery dir (HOME and BOSS_RECOVERY_DIR both unset); skipping",
        );
        return None;
    };
    match backup_execution_patch(&recovery_dir, workspace_path, &execution.id) {
        Ok(Some(path)) => {
            tracing::info!(
                execution_id = %execution.id,
                patch = %path.display(),
                "recovery-backup: captured uncommitted workspace work to patch",
            );
            Some(path)
        }
        Ok(None) => {
            tracing::debug!(
                execution_id = %execution.id,
                "recovery-backup: workspace diff is empty; nothing to back up",
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "recovery-backup: failed to capture workspace patch (non-fatal)",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── default_recovery_dir ──────────────────────────────────────

    /// The `BOSS_RECOVERY_DIR` override wins over the HOME-derived path.
    /// Guarded by a process-global env lock since env is process-wide.
    #[test]
    fn env_override_takes_precedence() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(RECOVERY_DIR_ENV);
        unsafe { std::env::set_var(RECOVERY_DIR_ENV, "/tmp/boss-recovery-test") };
        let resolved = default_recovery_dir();
        match prev {
            Some(v) => unsafe { std::env::set_var(RECOVERY_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(RECOVERY_DIR_ENV) },
        }
        assert_eq!(resolved, Some(PathBuf::from("/tmp/boss-recovery-test")));
    }

    // ── patch_file_name ───────────────────────────────────────────

    #[test]
    fn patch_file_name_keeps_well_formed_execution_id() {
        assert_eq!(
            patch_file_name("exec_18b434effe0b8340_b"),
            "exec_18b434effe0b8340_b.patch"
        );
    }

    #[test]
    fn patch_file_name_sanitizes_path_separators_and_traversal() {
        // A `/` or `..` in the id must never escape the recovery dir.
        let name = patch_file_name("../../etc/passwd");
        assert_eq!(name, "______etc_passwd.patch");
        assert!(!name.contains('/'));
        assert!(!name.contains(".."));
    }

    // ── write_patch_if_nonempty ───────────────────────────────────

    #[test]
    fn empty_diff_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let result = write_patch_if_nonempty(dir.path(), "exec_1", "   \n\t\n").unwrap();
        assert!(result.is_none(), "whitespace-only diff must be treated as empty");
        // Recovery dir must not even be populated with a stray file.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty(), "no patch file should be written for an empty diff");
    }

    #[test]
    fn nonempty_diff_writes_patch_with_expected_name_and_contents() {
        let dir = TempDir::new().unwrap();
        let diff = "diff --git a/foo b/foo\n+added line\n";
        let path = write_patch_if_nonempty(dir.path(), "exec_abc_3", diff)
            .unwrap()
            .expect("a non-empty diff must produce a patch path");
        assert_eq!(path, dir.path().join("exec_abc_3.patch"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, diff, "patch contents must match the captured diff verbatim");
    }

    #[test]
    fn write_patch_creates_missing_recovery_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("recovery").join("nested");
        let path = write_patch_if_nonempty(&nested, "exec_x", "diff --git a/x b/x\n+y\n")
            .unwrap()
            .expect("patch should be written");
        assert!(path.exists());
        assert!(nested.is_dir(), "missing recovery dir must be created");
    }

    // ── backup_dead_execution ─────────────────────────────────────

    /// An execution with no recorded workspace path is skipped cleanly
    /// (returns None) without touching the filesystem or invoking jj.
    #[test]
    fn backup_dead_execution_skips_when_no_workspace_path() {
        let execution = sample_execution(None);
        assert!(backup_dead_execution(&execution).is_none());
    }

    /// A recorded-but-nonexistent workspace path is skipped cleanly.
    #[test]
    fn backup_dead_execution_skips_when_workspace_missing() {
        let execution =
            sample_execution(Some("/nonexistent/boss/workspace/path".to_owned()));
        assert!(backup_dead_execution(&execution).is_none());
    }

    // ── end-to-end against a real jj workspace ────────────────────

    /// Full path: create a real jj workspace with an uncommitted edit and
    /// confirm [`backup_execution_patch`] captures it to a patch file.
    /// Skips gracefully when `jj` is not available on PATH (e.g. a
    /// hermetic CI sandbox) — the pure-logic tests above cover the rest.
    #[test]
    fn captures_uncommitted_work_from_real_jj_workspace() {
        if !jj_available() {
            eprintln!("skipping: `jj` not available on PATH");
            return;
        }
        let ws = TempDir::new().unwrap();
        if !init_jj_repo(ws.path()) {
            eprintln!("skipping: `jj git init` failed in test sandbox");
            return;
        }
        // Make an uncommitted edit in the working copy.
        std::fs::write(ws.path().join("hello.txt"), "uncommitted work\n").unwrap();

        let recovery = TempDir::new().unwrap();
        let patch = backup_execution_patch(recovery.path(), ws.path(), "exec_real_1")
            .expect("capture should succeed")
            .expect("dirty working copy should yield a patch");

        assert_eq!(patch, recovery.path().join("exec_real_1.patch"));
        let contents = std::fs::read_to_string(&patch).unwrap();
        assert!(
            contents.contains("hello.txt"),
            "patch should reference the changed file; got: {contents}"
        );
        assert!(
            contents.contains("uncommitted work"),
            "patch should include the added line; got: {contents}"
        );
    }

    /// A clean jj workspace (no uncommitted edits) yields no patch.
    #[test]
    fn clean_jj_workspace_yields_no_patch() {
        if !jj_available() {
            eprintln!("skipping: `jj` not available on PATH");
            return;
        }
        let ws = TempDir::new().unwrap();
        if !init_jj_repo(ws.path()) {
            eprintln!("skipping: `jj git init` failed in test sandbox");
            return;
        }
        let recovery = TempDir::new().unwrap();
        let patch =
            backup_execution_patch(recovery.path(), ws.path(), "exec_clean_1").unwrap();
        assert!(patch.is_none(), "a clean working copy must not produce a patch");
    }

    // ── helpers ───────────────────────────────────────────────────

    /// Process-global lock serialising tests that mutate env vars.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn sample_execution(workspace_path: Option<String>) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_test_1")
            .work_item_id("task_1")
            .kind(ExecutionKind::ChoreImplementation)
            .status("orphaned")
            .repo_remote_url("https://github.com/test/repo")
            .maybe_workspace_path(workspace_path)
            .created_at("0")
            .build()
    }

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Initialise a colocated jj repo at `path`. Returns false if jj
    /// refuses (so the caller can skip rather than fail).
    fn init_jj_repo(path: &Path) -> bool {
        Command::new("jj")
            .args(["git", "init"])
            .current_dir(path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
