//! Log-source -> on-disk path resolution, including the audit-path env
//! override and the default Boss state root.

use std::path::{Path, PathBuf};

/// Environment variable that overrides the audit-log path. Honoured by both
/// the engine (writer) and `bossctl` (reader) so they always agree on which
/// file they are talking about. Owning the constant here is what lets
/// `bossctl` resolve the audit path without depending on the engine crate.
pub const AUDIT_PATH_ENV: &str = "BOSS_ENGINE_AUDIT_PATH";

/// Filename of the structured engine trace log under the state root.
pub const ENGINE_TRACE_FILENAME: &str = "engine-trace.jsonl";

/// Filename of the engine lifecycle audit log under the state root.
pub const ENGINE_AUDIT_FILENAME: &str = "engine-audit.log";

/// Which engine log file a reader is targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// `engine-trace.jsonl` — structured tracing events (primary log).
    EngineTrace,
    /// `engine-audit.log` — lifecycle events (start, socket bind, shutdown).
    Audit,
}

impl LogSource {
    /// The bare filename this source resolves to under a state root.
    pub fn filename(self) -> &'static str {
        match self {
            LogSource::EngineTrace => ENGINE_TRACE_FILENAME,
            LogSource::Audit => ENGINE_AUDIT_FILENAME,
        }
    }
}

/// The default Boss state root: `$HOME/Library/Application Support/Boss`.
/// Returns `None` when `HOME` is unset.
pub fn default_state_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss"))
}

/// The audit-path override from [`AUDIT_PATH_ENV`], if set to a non-empty
/// (after trimming) value. Mirrors the trim/empty handling the engine and
/// CLI both relied on before this crate consolidated it.
pub fn audit_path_override() -> Option<PathBuf> {
    let raw = std::env::var_os(AUDIT_PATH_ENV)?;
    let trimmed = raw.to_string_lossy().trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Resolve a [`LogSource`] to its on-disk path under `state_root`. The audit
/// log honours [`AUDIT_PATH_ENV`]; the trace log is always
/// `<state_root>/engine-trace.jsonl`.
pub fn resolve_log_source_path(source: LogSource, state_root: &Path) -> PathBuf {
    match source {
        LogSource::Audit => audit_path_override().unwrap_or_else(|| state_root.join(ENGINE_AUDIT_FILENAME)),
        LogSource::EngineTrace => state_root.join(ENGINE_TRACE_FILENAME),
    }
}

/// Resolve the default audit-log path: [`AUDIT_PATH_ENV`] if set, otherwise
/// `<default_state_root>/engine-audit.log`. Returns `None` only when neither
/// the override nor `HOME` is available.
pub fn default_audit_log_path() -> Option<PathBuf> {
    if let Some(path) = audit_path_override() {
        return Some(path);
    }
    Some(default_state_root()?.join(ENGINE_AUDIT_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global `AUDIT_PATH_ENV`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn log_source_filenames_match_constants() {
        assert_eq!(LogSource::EngineTrace.filename(), "engine-trace.jsonl");
        assert_eq!(LogSource::Audit.filename(), "engine-audit.log");
    }

    #[test]
    fn engine_trace_resolves_under_state_root() {
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::EngineTrace, root),
            root.join("engine-trace.jsonl")
        );
    }

    #[test]
    fn audit_resolves_under_state_root_without_override() {
        let _guard = lock_env();
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            root.join("engine-audit.log")
        );
    }

    #[test]
    fn audit_override_wins_and_is_trimmed() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var(AUDIT_PATH_ENV, "  /custom/audit.log  ");
        }
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            PathBuf::from("/custom/audit.log")
        );
        assert_eq!(default_audit_log_path(), Some(PathBuf::from("/custom/audit.log")));
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
    }

    #[test]
    fn empty_override_is_ignored() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var(AUDIT_PATH_ENV, "   ");
        }
        assert_eq!(audit_path_override(), None);
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            root.join("engine-audit.log")
        );
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
    }
}
