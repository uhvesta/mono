use std::path::PathBuf;

use crate::app::CubeError;

pub fn data_dir() -> Result<PathBuf, CubeError> {
    if let Some(path) = std::env::var_os("CUBE_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("cube"));
    }

    let home = std::env::var_os("HOME").ok_or_else(|| {
        CubeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set",
        ))
    })?;
    Ok(PathBuf::from(home).join(".local/share/cube"))
}

pub fn database_path() -> Result<PathBuf, CubeError> {
    Ok(data_dir()?.join("state.db"))
}
