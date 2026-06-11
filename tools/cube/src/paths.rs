use std::path::PathBuf;

use crate::app::CubeError;

pub fn data_dir() -> Result<PathBuf, CubeError> {
    if let Some(path) = std::env::var_os("CUBE_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("cube"));
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| CubeError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set")))?;
    Ok(PathBuf::from(home).join(".local/share/cube"))
}

pub fn database_path() -> Result<PathBuf, CubeError> {
    Ok(data_dir()?.join("state.db"))
}

pub fn repo_lock_path(repo: &str) -> Result<PathBuf, CubeError> {
    Ok(repo_lock_path_in(&data_dir()?, repo))
}

pub fn repo_lock_path_in(data_dir: &std::path::Path, repo: &str) -> PathBuf {
    data_dir.join("locks").join(format!("{repo}.lock"))
}

pub fn audit_dir() -> Result<PathBuf, CubeError> {
    Ok(audit_dir_in(&data_dir()?))
}

pub fn audit_dir_in(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("audit")
}

pub fn workspace_logs_dir() -> Result<PathBuf, CubeError> {
    Ok(workspace_logs_dir_in(&data_dir()?))
}

pub fn workspace_logs_dir_in(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("workspace-logs")
}

pub fn workspace_logs_path(workspace_id: &str) -> Result<PathBuf, CubeError> {
    Ok(workspace_logs_dir()?.join(workspace_id))
}

pub fn workspace_logs_path_in(data_dir: &std::path::Path, workspace_id: &str) -> PathBuf {
    workspace_logs_dir_in(data_dir).join(workspace_id)
}
