use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};

use crate::coordinator::MAX_WORKER_POOL_SIZE;

const DEFAULT_CUBE_COMMAND: &str = "bazel run //tools/cube:cube --";

#[derive(Debug, Clone)]
pub struct CubeConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkConfig {
    pub cwd: PathBuf,
    pub db_path: PathBuf,
    pub worker_pool_size: usize,
}

impl WorkConfig {
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
        Ok(Self {
            cwd,
            db_path,
            worker_pool_size,
        })
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
            std::env::var("BOSS_CUBE_CMD").unwrap_or_else(|_| DEFAULT_CUBE_COMMAND.to_owned()),
        )?;

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
    use super::{MAX_WORKER_POOL_SIZE, WorkConfig, resolve_runtime_cwd};
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
}
