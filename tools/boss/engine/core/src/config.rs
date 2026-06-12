use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};

use crate::coordinator::{DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_WORKER_POOL_SIZE};

/// Default value for [`WorkConfig::max_review_cycles`]. Matches the
/// "~3 cycles at worst" mental model from P992 design §7.
pub const DEFAULT_MAX_REVIEW_CYCLES: usize = 3;

/// Default threshold for the no-op / trivial-diff skip gate (P992 design §8).
/// Zero means "skip only when the effective diff is literally empty (0 changed
/// lines)"; operators can raise this to also skip small cosmetic-only pushes.
pub const DEFAULT_MIN_REVIEW_CHANGED_LINES: u64 = 0;

/// Default line threshold for embedding `gh pr diff` output directly in the
/// reviewer's initial prompt. PRs whose diff is at or below this many lines
/// get the diff pre-embedded so the reviewer skips one `gh pr diff` tool
/// call. Set to 0 to disable embedding entirely. Operators can lower this for
/// cost-sensitive deployments or raise it to cover larger PRs.
pub const DEFAULT_MAX_EMBED_DIFF_LINES: u64 = 500;

// Bare name used as the PATH fallback. In installed Boss.app the engine
// resolves cube from the bundle first (see resolve_cube_command); this
// constant is only reached in dev mode or when the bundle copy is absent.
const DEFAULT_CUBE_COMMAND: &str = "cube";

#[derive(Debug, Clone)]
pub struct CubeConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
#[non_exhaustive]
pub struct WorkConfig {
    pub cwd: PathBuf,
    pub db_path: PathBuf,
    /// Defaults to 1 so test call sites don't need updating when new pool
    /// fields are added.
    #[builder(default = 1)]
    pub worker_pool_size: usize,
    /// Size of the dedicated automation worker pool. Configured via
    /// `BOSS_AUTOMATION_POOL_SIZE`; defaults to [`MAX_AUTOMATION_POOL_SIZE`].
    #[builder(default = 1)]
    pub automation_pool_size: usize,
    /// Size of the dedicated review worker pool. Configured via
    /// `BOSS_REVIEW_POOL_SIZE`; defaults to [`DEFAULT_REVIEW_POOL_SIZE`]
    /// (deliberately small to bound always-Opus review spend).
    #[builder(default = 1)]
    pub review_pool_size: usize,
    /// Maximum number of automated reviewer passes to run per PR.
    /// When a producing task's `review_cycle` reaches this value the engine
    /// skips the next reviewer pass and advances the task to human Review
    /// directly. Configured via `BOSS_MAX_REVIEW_CYCLES`; defaults to
    /// [`DEFAULT_MAX_REVIEW_CYCLES`] (3). P992 design §7.
    #[builder(default = DEFAULT_MAX_REVIEW_CYCLES)]
    pub max_review_cycles: usize,
    /// Minimum number of changed lines (additions + deletions) required to
    /// trigger a reviewer pass when `last_reviewed_sha` is set. Pushes whose
    /// effective diff (new head vs. last-reviewed head) totals fewer lines
    /// than this threshold are skipped as trivial. Zero (the default) means
    /// skip only when the diff is completely empty; operators can raise it to
    /// also skip small cosmetic pushes. Configured via
    /// `BOSS_MIN_REVIEW_CHANGED_LINES`; defaults to
    /// [`DEFAULT_MIN_REVIEW_CHANGED_LINES`] (0). P992 design §8.
    #[builder(default = DEFAULT_MIN_REVIEW_CHANGED_LINES)]
    pub min_review_changed_lines: u64,
    /// Maximum diff size (lines) at which the engine pre-embeds the full
    /// `gh pr diff` output in the reviewer's initial prompt. PRs at or below
    /// this threshold skip the reviewer's first `gh pr diff` tool call.
    /// Set to 0 to disable embedding. Configured via
    /// `BOSS_MAX_EMBED_DIFF_LINES`; defaults to
    /// [`DEFAULT_MAX_EMBED_DIFF_LINES`] (500).
    #[builder(default = DEFAULT_MAX_EMBED_DIFF_LINES)]
    pub max_review_embed_diff_lines: u64,
}

impl WorkConfig {
    pub fn load_from_env() -> Result<Self> {
        Self::load_from(|k| std::env::var_os(k))
    }

    /// Load config from an explicit env lookup rather than the process
    /// environment. Tests call this directly so they never mutate global state.
    pub fn load_from(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let cwd = resolve_runtime_cwd_with(&lookup)?;
        let db_path = match lookup("BOSS_DB_PATH") {
            Some(path) => PathBuf::from(path),
            None => default_db_path()?,
        };
        // Default to the hard cap so the engine pool tracks the macOS
        // app's slot count (`WorkersWorkspaceModel.workerSlotCount = 8`).
        // A smaller default left slots 5–8 idle while the dispatcher
        // silently no-op'd new work. `BOSS_WORKER_POOL_SIZE` still
        // overrides for callers that genuinely want fewer workers.
        let worker_pool_size = lookup_usize(&lookup, "BOSS_WORKER_POOL_SIZE")?.unwrap_or(MAX_WORKER_POOL_SIZE);
        let automation_pool_size =
            lookup_usize(&lookup, "BOSS_AUTOMATION_POOL_SIZE")?.unwrap_or(MAX_AUTOMATION_POOL_SIZE);
        let review_pool_size = lookup_usize(&lookup, "BOSS_REVIEW_POOL_SIZE")?.unwrap_or(DEFAULT_REVIEW_POOL_SIZE);
        let max_review_cycles = lookup_usize(&lookup, "BOSS_MAX_REVIEW_CYCLES")?.unwrap_or(DEFAULT_MAX_REVIEW_CYCLES);
        let min_review_changed_lines =
            lookup_u64(&lookup, "BOSS_MIN_REVIEW_CHANGED_LINES")?.unwrap_or(DEFAULT_MIN_REVIEW_CHANGED_LINES);
        let max_review_embed_diff_lines =
            lookup_u64(&lookup, "BOSS_MAX_EMBED_DIFF_LINES")?.unwrap_or(DEFAULT_MAX_EMBED_DIFF_LINES);
        Ok(WorkConfig::builder()
            .cwd(cwd)
            .db_path(db_path)
            .worker_pool_size(worker_pool_size)
            .automation_pool_size(automation_pool_size)
            .review_pool_size(review_pool_size)
            .max_review_cycles(max_review_cycles)
            .min_review_changed_lines(min_review_changed_lines)
            .max_review_embed_diff_lines(max_review_embed_diff_lines)
            .build())
    }
}

fn lookup_usize(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<usize>> {
    match lookup(name) {
        None => Ok(None),
        Some(val) => {
            let raw = val.to_string_lossy().into_owned();
            raw.parse::<usize>()
                .with_context(|| format!("could not parse {name}: {raw}"))
                .map(Some)
        }
    }
}

fn lookup_u64(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<u64>> {
    match lookup(name) {
        None => Ok(None),
        Some(val) => {
            let raw = val.to_string_lossy().into_owned();
            raw.parse::<u64>()
                .with_context(|| format!("could not parse {name}: {raw}"))
                .map(Some)
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub anthropic_api_key: Option<String>,
    pub cube: CubeConfig,
    pub cwd: PathBuf,
}

impl AgentConfig {
    pub fn load_from_env(work: &WorkConfig) -> Result<Self> {
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();

        let (cube_command, cube_args) = parse_command_line(
            "BOSS_CUBE_CMD",
            std::env::var("BOSS_CUBE_CMD").unwrap_or_else(|_| resolve_cube_command()),
        )?;

        log_cube_resolution(&cube_command);

        Ok(Self {
            anthropic_api_key,
            cube: CubeConfig {
                command: cube_command,
                args: cube_args,
            },
            cwd: work.cwd.clone(),
        })
    }
}

#[derive(Debug)]
pub struct RuntimeConfig {
    pub work: WorkConfig,
    agent_cell: OnceLock<Arc<AgentConfig>>,
}

impl RuntimeConfig {
    pub fn load_from_env() -> Result<Self> {
        Ok(Self {
            work: WorkConfig::load_from_env()?,
            agent_cell: OnceLock::new(),
        })
    }

    pub fn from_parts(work: WorkConfig, agent: Option<AgentConfig>) -> Self {
        let cell = OnceLock::new();
        if let Some(agent) = agent {
            let _ = cell.set(Arc::new(agent));
        }
        Self { work, agent_cell: cell }
    }

    pub fn agent(&self) -> Result<Arc<AgentConfig>> {
        if let Some(agent) = self.agent_cell.get() {
            return Ok(agent.clone());
        }
        let loaded = AgentConfig::load_from_env(&self.work)?;
        let arc = Arc::new(loaded);
        match self.agent_cell.set(arc.clone()) {
            Ok(()) => Ok(arc),
            Err(_) => Ok(self.agent_cell.get().expect("OnceLock set after failed insert").clone()),
        }
    }
}

/// Returns the cube command to use, preferring a bundle-relative binary when
/// the engine itself was launched from a bundle (installed Boss.app).
///
/// Resolution order:
///   1. `<engine_exe_dir>/cube` — present in the bundle; used by installed
///      Boss.app so the engine never depends on the GUI launchd PATH.
///   2. `"cube"` — bare name resolved from PATH at exec time; used in dev
///      mode where the engine runs via `bazel run` outside a bundle.
///
/// Workers run inside Ghostty terminal panes which inherit the user's shell
/// PATH, so they continue to resolve cube (and jj, gh, claude, etc.) from
/// PATH naturally. This bundle-relative lookup is engine-only.
fn resolve_cube_command() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(DEFAULT_CUBE_COMMAND);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    DEFAULT_CUBE_COMMAND.to_owned()
}

/// Logs how cube was resolved and warns if the bare name cannot be found on PATH.
fn log_cube_resolution(command: &str) {
    if command.contains('/') {
        tracing::info!(command, "cube resolved from bundle");
        return;
    }
    let path_env = std::env::var("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path_env).any(|dir| dir.join(command).is_file());
    if found {
        tracing::info!(command, "cube resolved from PATH");
    } else {
        tracing::warn!(
            command,
            "cube executable not found on PATH; worker dispatch will fail — \
             install cube or set BOSS_CUBE_CMD to its full path"
        );
    }
}

fn parse_command_line(env_var: &str, command_line: String) -> Result<(String, Vec<String>)> {
    let parts = shlex::split(&command_line).with_context(|| format!("could not parse {env_var}: {command_line}"))?;

    let Some((command, args)) = parts.split_first() else {
        bail!("{env_var} resolved to an empty command");
    };

    Ok((command.clone(), args.to_vec()))
}

fn resolve_runtime_cwd_with(lookup: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = lookup("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    std::env::current_dir().context("failed to resolve current working directory")
}

fn default_db_path() -> Result<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        bail!("HOME must be set to derive the default Boss database path");
    };

    Ok(PathBuf::from(home).join("Library/Application Support/Boss/state.db"))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_EMBED_DIFF_LINES, DEFAULT_MAX_REVIEW_CYCLES, DEFAULT_MIN_REVIEW_CHANGED_LINES,
        DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_WORKER_POOL_SIZE, WorkConfig,
    };
    use std::ffi::OsString;

    #[test]
    fn prefers_bazel_workspace_directory_when_present() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");
        // Can't use env_map here because tempdir paths are runtime values,
        // so build the closure directly.
        let config = WorkConfig::load_from(|k| match k {
            "BUILD_WORKSPACE_DIRECTORY" => Some(OsString::from(tempdir.path())),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path)),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.cwd, tempdir.path());
    }

    /// `WorkConfig::load_from` must default to the hard cap
    /// (`MAX_WORKER_POOL_SIZE`) when `BOSS_WORKER_POOL_SIZE` is absent,
    /// matching the macOS app's slot count. A lower default left
    /// slots 5–8 unallocated and silently dropped any drag-to-Doing
    /// dispatch once slots 1–4 were busy.
    #[test]
    fn worker_pool_size_defaults_to_max_when_env_unset() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.worker_pool_size, MAX_WORKER_POOL_SIZE);
    }

    // Default-and-override are checked in a single test (rather than the
    // two-test pattern) so the two cases can't run in parallel and race on
    // the shared process-global `BOSS_AUTOMATION_POOL_SIZE`: `config::tests`
    // all land in the multi-threaded `engine_lib_test_rest` shard. (Same
    // rationale as `review_pool_size_defaults_and_reads_from_env` below.)
    #[test]
    fn automation_pool_size_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the max default.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.automation_pool_size, MAX_AUTOMATION_POOL_SIZE);

        // Set → the env value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_AUTOMATION_POOL_SIZE" => Some(OsString::from("2")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.automation_pool_size, 2);
    }

    #[test]
    fn review_pool_size_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the small default.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.review_pool_size, DEFAULT_REVIEW_POOL_SIZE);

        // Present → the explicit value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_REVIEW_POOL_SIZE" => Some(OsString::from("1")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.review_pool_size, 1);
    }

    #[test]
    fn min_review_changed_lines_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.min_review_changed_lines, DEFAULT_MIN_REVIEW_CHANGED_LINES);

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MIN_REVIEW_CHANGED_LINES" => Some(OsString::from("10")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.min_review_changed_lines, 10);
    }

    #[test]
    fn max_review_cycles_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the hardcoded default (3).
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_cycles, DEFAULT_MAX_REVIEW_CYCLES);

        // Present → the explicit value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MAX_REVIEW_CYCLES" => Some(OsString::from("5")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_cycles, 5);
    }

    #[test]
    fn max_embed_diff_lines_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_embed_diff_lines, DEFAULT_MAX_EMBED_DIFF_LINES);

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MAX_EMBED_DIFF_LINES" => Some(OsString::from("200")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_embed_diff_lines, 200);
    }
}
