use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};

use crate::coordinator::{
    DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_WORKER_POOL_SIZE,
};

// Bare name used as the PATH fallback. In installed Boss.app the engine
// resolves cube from the bundle first (see resolve_cube_command); this
// constant is only reached in dev mode or when the bundle copy is absent.
const DEFAULT_CUBE_COMMAND: &str = "cube";

#[derive(Debug, Clone)]
pub struct CubeConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WorkConfig {
    pub cwd: PathBuf,
    pub db_path: PathBuf,
    pub worker_pool_size: usize,
    /// Size of the dedicated automation worker pool. Configured via
    /// `BOSS_AUTOMATION_POOL_SIZE`; defaults to [`MAX_AUTOMATION_POOL_SIZE`].
    pub automation_pool_size: usize,
    /// Size of the dedicated review worker pool. Configured via
    /// `BOSS_REVIEW_POOL_SIZE`; defaults to [`DEFAULT_REVIEW_POOL_SIZE`]
    /// (deliberately small to bound always-Opus review spend).
    pub review_pool_size: usize,
}

impl WorkConfig {
    /// Start building a [`WorkConfig`]. `cwd` and `db_path` are required; all
    /// pool sizes default to 1 so call sites (especially tests) don't have to
    /// be updated every time a new pool field is added.
    pub fn builder() -> WorkConfigBuilder {
        WorkConfigBuilder::new()
    }

    pub fn load_from_env() -> Result<Self> {
        let cwd = resolve_runtime_cwd()?;
        let db_path = match std::env::var_os("BOSS_DB_PATH") {
            Some(path) => PathBuf::from(path),
            None => default_db_path()?,
        };
        // Default to the hard cap so the engine pool tracks the macOS
        // app's slot count (`WorkersWorkspaceModel.workerSlotCount = 8`).
        // A smaller default left slots 5–8 idle while the dispatcher
        // silently no-op'd new work. `BOSS_WORKER_POOL_SIZE` still
        // overrides for callers that genuinely want fewer workers.
        let worker_pool_size = std::env::var("BOSS_WORKER_POOL_SIZE")
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .with_context(|| format!("could not parse BOSS_WORKER_POOL_SIZE: {raw}"))
            })
            .transpose()?
            .unwrap_or(MAX_WORKER_POOL_SIZE);
        let automation_pool_size = std::env::var("BOSS_AUTOMATION_POOL_SIZE")
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .with_context(|| format!("could not parse BOSS_AUTOMATION_POOL_SIZE: {raw}"))
            })
            .transpose()?
            .unwrap_or(MAX_AUTOMATION_POOL_SIZE);
        let review_pool_size = std::env::var("BOSS_REVIEW_POOL_SIZE")
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .with_context(|| format!("could not parse BOSS_REVIEW_POOL_SIZE: {raw}"))
            })
            .transpose()?
            .unwrap_or(DEFAULT_REVIEW_POOL_SIZE);
        Ok(WorkConfig::builder()
            .cwd(cwd)
            .db_path(db_path)
            .worker_pool_size(worker_pool_size)
            .automation_pool_size(automation_pool_size)
            .review_pool_size(review_pool_size)
            .build())
    }
}

/// Builder for [`WorkConfig`]. Pool sizes default to 1; `cwd` and `db_path`
/// must be set before [`build`](WorkConfigBuilder::build).
#[derive(Debug, Clone, Default)]
pub struct WorkConfigBuilder {
    cwd: Option<PathBuf>,
    db_path: Option<PathBuf>,
    worker_pool_size: Option<usize>,
    automation_pool_size: Option<usize>,
    review_pool_size: Option<usize>,
}

impl WorkConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn db_path(mut self, db_path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(db_path.into());
        self
    }

    pub fn worker_pool_size(mut self, size: usize) -> Self {
        self.worker_pool_size = Some(size);
        self
    }

    pub fn automation_pool_size(mut self, size: usize) -> Self {
        self.automation_pool_size = Some(size);
        self
    }

    pub fn review_pool_size(mut self, size: usize) -> Self {
        self.review_pool_size = Some(size);
        self
    }

    /// Build the [`WorkConfig`]. Panics if `cwd` or `db_path` were not set.
    pub fn build(self) -> WorkConfig {
        WorkConfig {
            cwd: self.cwd.expect("WorkConfig::builder requires cwd"),
            db_path: self.db_path.expect("WorkConfig::builder requires db_path"),
            worker_pool_size: self.worker_pool_size.unwrap_or(1),
            automation_pool_size: self.automation_pool_size.unwrap_or(1),
            review_pool_size: self.review_pool_size.unwrap_or(1),
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
        Self {
            work,
            agent_cell: cell,
        }
    }

    pub fn agent(&self) -> Result<Arc<AgentConfig>> {
        if let Some(agent) = self.agent_cell.get() {
            return Ok(agent.clone());
        }
        let loaded = AgentConfig::load_from_env(&self.work)?;
        let arc = Arc::new(loaded);
        match self.agent_cell.set(arc.clone()) {
            Ok(()) => Ok(arc),
            Err(_) => Ok(self
                .agent_cell
                .get()
                .expect("OnceLock set after failed insert")
                .clone()),
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
        && let Some(dir) = exe.parent() {
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
    let parts = shlex::split(&command_line)
        .with_context(|| format!("could not parse {env_var}: {command_line}"))?;

    let Some((command, args)) = parts.split_first() else {
        bail!("{env_var} resolved to an empty command");
    };

    Ok((command.clone(), args.to_vec()))
}

fn resolve_runtime_cwd() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
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
        DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_WORKER_POOL_SIZE, WorkConfig,
        resolve_runtime_cwd,
    };
    use std::path::PathBuf;

    #[test]
    fn prefers_bazel_workspace_directory_when_present() {
        let original = std::env::var_os("BUILD_WORKSPACE_DIRECTORY");
        let tempdir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("BUILD_WORKSPACE_DIRECTORY", tempdir.path());
        }

        let cwd = resolve_runtime_cwd().unwrap();
        assert_eq!(cwd, PathBuf::from(tempdir.path()));

        match original {
            Some(value) => unsafe {
                std::env::set_var("BUILD_WORKSPACE_DIRECTORY", value);
            },
            None => unsafe {
                std::env::remove_var("BUILD_WORKSPACE_DIRECTORY");
            },
        }
    }

    /// `WorkConfig::load_from_env` must default to the hard cap
    /// (`MAX_WORKER_POOL_SIZE`) when `BOSS_WORKER_POOL_SIZE` is unset,
    /// matching the macOS app's slot count. A lower default left
    /// slots 5–8 unallocated and silently dropped any drag-to-Doing
    /// dispatch once slots 1–4 were busy.
    #[test]
    fn worker_pool_size_defaults_to_max_when_env_unset() {
        // Force the test to take the unset branch even when the host
        // shell exports a custom pool size.
        let original_pool = std::env::var_os("BOSS_WORKER_POOL_SIZE");
        let original_db = std::env::var_os("BOSS_DB_PATH");
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");
        unsafe {
            std::env::remove_var("BOSS_WORKER_POOL_SIZE");
            std::env::set_var("BOSS_DB_PATH", &db_path);
        }

        let config = WorkConfig::load_from_env().expect("config loads");
        assert_eq!(config.worker_pool_size, MAX_WORKER_POOL_SIZE);

        unsafe {
            match original_pool {
                Some(value) => std::env::set_var("BOSS_WORKER_POOL_SIZE", value),
                None => std::env::remove_var("BOSS_WORKER_POOL_SIZE"),
            }
            match original_db {
                Some(value) => std::env::set_var("BOSS_DB_PATH", value),
                None => std::env::remove_var("BOSS_DB_PATH"),
            }
        }
    }

    #[test]
    fn automation_pool_size_defaults_to_max_when_env_unset() {
        let original_pool = std::env::var_os("BOSS_AUTOMATION_POOL_SIZE");
        let original_db = std::env::var_os("BOSS_DB_PATH");
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");
        unsafe {
            std::env::remove_var("BOSS_AUTOMATION_POOL_SIZE");
            std::env::set_var("BOSS_DB_PATH", &db_path);
        }

        let config = WorkConfig::load_from_env().expect("config loads");
        assert_eq!(config.automation_pool_size, MAX_AUTOMATION_POOL_SIZE);

        unsafe {
            match original_pool {
                Some(value) => std::env::set_var("BOSS_AUTOMATION_POOL_SIZE", value),
                None => std::env::remove_var("BOSS_AUTOMATION_POOL_SIZE"),
            }
            match original_db {
                Some(value) => std::env::set_var("BOSS_DB_PATH", value),
                None => std::env::remove_var("BOSS_DB_PATH"),
            }
        }
    }

    #[test]
    fn automation_pool_size_reads_from_env() {
        let original_pool = std::env::var_os("BOSS_AUTOMATION_POOL_SIZE");
        let original_db = std::env::var_os("BOSS_DB_PATH");
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");
        unsafe {
            std::env::set_var("BOSS_AUTOMATION_POOL_SIZE", "2");
            std::env::set_var("BOSS_DB_PATH", &db_path);
        }

        let config = WorkConfig::load_from_env().expect("config loads");
        assert_eq!(config.automation_pool_size, 2);

        unsafe {
            match original_pool {
                Some(value) => std::env::set_var("BOSS_AUTOMATION_POOL_SIZE", value),
                None => std::env::remove_var("BOSS_AUTOMATION_POOL_SIZE"),
            }
            match original_db {
                Some(value) => std::env::set_var("BOSS_DB_PATH", value),
                None => std::env::remove_var("BOSS_DB_PATH"),
            }
        }
    }

    // Default-and-override are checked in a single test (rather than the
    // two-test pattern used elsewhere) so the two cases can't run in
    // parallel and race on the shared process-global `BOSS_REVIEW_POOL_SIZE`:
    // `config::tests` all land in the multi-threaded `engine_lib_test_rest`
    // shard.
    #[test]
    fn review_pool_size_defaults_and_reads_from_env() {
        let original_pool = std::env::var_os("BOSS_REVIEW_POOL_SIZE");
        let original_db = std::env::var_os("BOSS_DB_PATH");
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");

        // Unset → falls back to the small default.
        unsafe {
            std::env::remove_var("BOSS_REVIEW_POOL_SIZE");
            std::env::set_var("BOSS_DB_PATH", &db_path);
        }
        let config = WorkConfig::load_from_env().expect("config loads");
        assert_eq!(config.review_pool_size, DEFAULT_REVIEW_POOL_SIZE);

        // Set → the env value wins.
        unsafe {
            std::env::set_var("BOSS_REVIEW_POOL_SIZE", "1");
        }
        let config = WorkConfig::load_from_env().expect("config loads");
        assert_eq!(config.review_pool_size, 1);

        unsafe {
            match original_pool {
                Some(value) => std::env::set_var("BOSS_REVIEW_POOL_SIZE", value),
                None => std::env::remove_var("BOSS_REVIEW_POOL_SIZE"),
            }
            match original_db {
                Some(value) => std::env::set_var("BOSS_DB_PATH", value),
                None => std::env::remove_var("BOSS_DB_PATH"),
            }
        }
    }
}
