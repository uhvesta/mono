//! Engine-owned structured-output artifacts (chore: "Review findings + task
//! followups: file-artifact output instead of transcript scraping").
//!
//! Two worker paths produce structured JSON the engine consumes: **review
//! findings** (a reviewer's `ReviewResult`) and **task followups** (a
//! `Vec<FollowupEntry>`). Historically the engine *scraped* this JSON out of
//! the worker's transcript / final message — a fragile ladder of fenced-block
//! and balanced-brace heuristics where a malformed message silently dropped
//! the whole payload. This module replaces that with a written-file contract:
//! the engine designates a per-execution path, the worker writes its JSON
//! there, and the engine reads + schema-validates the file at the Stop
//! boundary (mirroring how the design-doc questions manifest already works,
//! except this artifact is engine-owned scratch rather than a committed
//! sidecar).
//!
//! # Why a temp dir, not the workspace or the Boss data dir
//!
//! The artifact must live **outside the worker's git checkout**: the reviewer
//! is read-only and must not commit a sidecar, and followup manifests must not
//! pollute the PR. It also cannot live under the Boss support dir, because
//! workers are fenced off that directory by both the permission deny-globs and
//! the `PreToolUse` path-guard hook (see [`crate::worker_setup`]). So it lives
//! in an engine-owned scratch directory under the system temp dir, keyed by
//! execution id, and is reaped after the engine reads it. The reviewer's
//! file-write deny is correspondingly scoped to its workspace (see
//! [`crate::worker_setup::reviewer_deny_rules`]) so it may write this one
//! artifact while still being unable to touch the PR/repo.

use std::path::{Path, PathBuf};

/// Env var carrying the absolute artifact path into the worker's environment.
///
/// The operative instruction is the literal absolute path embedded in the
/// worker prompt (a model writes via the `Write` tool, not by expanding env
/// vars); this var surfaces the same value so a worker can also resolve it
/// programmatically and so the convention is self-documenting in the pane env.
pub const STRUCTURED_OUTPUT_ENV: &str = "BOSS_STRUCTURED_OUTPUT";

/// Engine-side env var overriding the base directory. Set by tests and by
/// non-default installs; unset in production (the temp-dir default applies).
const OUTPUT_DIR_ENV: &str = "BOSS_WORKER_OUTPUT_DIR";

/// Fixed subdirectory created under the resolved base.
const DIR_NAME: &str = "boss-worker-output";

/// Resolve the engine-owned base directory for structured-output artifacts:
/// `$BOSS_WORKER_OUTPUT_DIR` when set and non-empty, else
/// `<system temp>/boss-worker-output`.
///
/// Read identically by the spawn path (which creates the dir + embeds the
/// path in the prompt) and the completion path (which reads + reaps the
/// file). Both run inside the single engine process, so the resolution is
/// consistent within a run.
pub fn default_dir() -> PathBuf {
    match std::env::var_os(OUTPUT_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(DIR_NAME),
    }
}

/// Absolute artifact path for `execution_id` under `dir`.
pub fn path_in(dir: &Path, execution_id: &str) -> PathBuf {
    dir.join(format!("{execution_id}.json"))
}

/// The default-dir artifact path for `execution_id`, as a string — the form
/// embedded in worker prompts and exported as [`STRUCTURED_OUTPUT_ENV`]. A
/// thin wrapper over [`default_dir`] + [`path_in`] so the spawn-side callers
/// (prompt text + env var) stay in lockstep with the completion-side reader,
/// which resolves the same [`default_dir`].
pub fn default_path_string(execution_id: &str) -> String {
    path_in(&default_dir(), execution_id).display().to_string()
}

/// Ensure `dir` exists and no stale artifact from a prior run of the same
/// execution id remains, then return the artifact path. A leftover file from
/// an earlier run of this exact execution id is removed so a re-prompted
/// worker that fails to re-write cannot have the engine read a stale result.
/// Failure to remove a stale file is logged and ignored — the worker
/// overwrites it anyway.
pub fn prepare(dir: &Path, execution_id: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = path_in(dir, execution_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                ?err,
                "structured-output: could not remove stale artifact (continuing)"
            );
        }
    }
    Ok(path)
}

/// Read the artifact for `execution_id` under `dir`, returning `None` when it
/// is absent or empty. An empty/whitespace-only file is treated as absent —
/// the worker never wrote a real payload.
pub fn read(dir: &Path, execution_id: &str) -> Option<String> {
    let path = path_in(dir, execution_id);
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                ?err,
                "structured-output: failed to read artifact"
            );
            None
        }
    }
}

/// Best-effort delete (reaping) of the artifact for `execution_id`. Never
/// errors — a missing file is the common case and any other failure is logged
/// and ignored (the system temp dir self-reaps as a backstop).
pub fn clear(dir: &Path, execution_id: &str) {
    let path = path_in(dir, execution_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                ?err,
                "structured-output: could not reap artifact"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_in_uses_execution_id_json() {
        let p = path_in(Path::new("/base"), "exec_abc_1");
        assert_eq!(p, PathBuf::from("/base/exec_abc_1.json"));
    }

    #[test]
    fn prepare_creates_dir_and_clears_stale() {
        let root = std::env::temp_dir().join(format!(
            "boss-so-test-prepare-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("nested");
        // Pre-seed a stale artifact so prepare must clear it.
        std::fs::create_dir_all(&dir).unwrap();
        let stale = path_in(&dir, "exec_1");
        std::fs::write(&stale, "STALE").unwrap();

        let path = prepare(&dir, "exec_1").unwrap();
        assert_eq!(path, stale);
        assert!(!stale.exists(), "prepare must remove the stale artifact");
        assert!(dir.is_dir(), "prepare must ensure the dir exists");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_roundtrips_written_content_and_treats_empty_as_absent() {
        let dir = std::env::temp_dir().join(format!(
            "boss-so-test-read-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(read(&dir, "missing"), None, "absent file → None");

        let path = path_in(&dir, "e1");
        std::fs::write(&path, "   \n  ").unwrap();
        assert_eq!(read(&dir, "e1"), None, "empty/whitespace file → None");

        std::fs::write(&path, r#"{"k":"v"}"#).unwrap();
        assert_eq!(read(&dir, "e1").as_deref(), Some(r#"{"k":"v"}"#));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clear_removes_file_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "boss-so-test-clear-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_in(&dir, "e1");
        std::fs::write(&path, "x").unwrap();

        clear(&dir, "e1");
        assert!(!path.exists(), "clear must remove the artifact");
        // Idempotent: clearing an already-absent file must not panic.
        clear(&dir, "e1");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
