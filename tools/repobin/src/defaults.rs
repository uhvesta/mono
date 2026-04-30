use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::RepobinError;

pub const DEFAULTS_FILE_NAME: &str = "repobin.yaml";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultsConfig {
    pub version: u32,
    #[serde(default)]
    pub tools: BTreeMap<String, DefaultsTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultsTool {
    pub repo: String,
}

#[derive(Debug, Clone)]
pub struct LoadedDefaults {
    pub path: PathBuf,
    pub config: DefaultsConfig,
}

impl DefaultsConfig {
    pub fn empty() -> Self {
        Self {
            version: SUPPORTED_VERSION,
            tools: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), RepobinError> {
        if self.version != SUPPORTED_VERSION {
            return Err(RepobinError::UnsupportedDefaultsVersion {
                version: self.version,
            });
        }
        for (name, tool) in &self.tools {
            if tool.repo.trim().is_empty() {
                return Err(RepobinError::InvalidDefaults(format!(
                    "tool `{name}` must declare a non-empty repo URL"
                )));
            }
        }
        Ok(())
    }
}

pub fn defaults_path(repobin_exe: &Path) -> PathBuf {
    repobin_exe
        .parent()
        .map(|parent| parent.join(DEFAULTS_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(DEFAULTS_FILE_NAME))
}

pub fn load_defaults_at(path: &Path) -> Result<Option<LoadedDefaults>, RepobinError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RepobinError::ReadDefaults {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let config: DefaultsConfig =
        serde_yaml::from_str(&raw).map_err(|source| RepobinError::ParseDefaults {
            path: path.to_path_buf(),
            source,
        })?;
    config.validate()?;
    Ok(Some(LoadedDefaults {
        path: path.to_path_buf(),
        config,
    }))
}

pub fn load_defaults_for_exe(repobin_exe: &Path) -> Result<Option<LoadedDefaults>, RepobinError> {
    let candidate = defaults_path(repobin_exe);
    load_defaults_at(&candidate)
}

pub fn write_defaults(path: &Path, config: &DefaultsConfig) -> Result<(), RepobinError> {
    config.validate()?;
    let serialized =
        serde_yaml::to_string(config).map_err(|source| RepobinError::SerializeDefaults {
            path: path.to_path_buf(),
            source,
        })?;
    std::fs::write(path, serialized).map_err(|source| RepobinError::WriteDefaults {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::TempDir;

    use super::{
        DEFAULTS_FILE_NAME, DefaultsConfig, DefaultsTool, defaults_path, load_defaults_at,
        write_defaults,
    };

    #[test]
    fn defaults_path_sits_next_to_binary() {
        let path = defaults_path(std::path::Path::new("/Users/test/bin/repobin"));
        assert_eq!(
            path,
            std::path::Path::new("/Users/test/bin").join(DEFAULTS_FILE_NAME)
        );
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("repobin.yaml");
        let loaded = load_defaults_at(&path).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn round_trips_through_yaml() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("repobin.yaml");
        let mut tools = BTreeMap::new();
        tools.insert(
            "boss".to_string(),
            DefaultsTool {
                repo: "https://example.com/mono.git".to_string(),
            },
        );
        let config = DefaultsConfig { version: 1, tools };
        write_defaults(&path, &config).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("boss"));
        assert!(raw.contains("https://example.com/mono.git"));

        let loaded = load_defaults_at(&path).unwrap().unwrap();
        assert_eq!(loaded.config, config);
    }

    #[test]
    fn rejects_empty_repo() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("repobin.yaml");
        fs::write(&path, "version: 1\ntools:\n  boss:\n    repo: \"\"\n").unwrap();
        let err = load_defaults_at(&path).unwrap_err();
        assert!(
            err.to_string().contains("non-empty repo URL"),
            "got error: {err}"
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("repobin.yaml");
        fs::write(&path, "version: 99\ntools: {}\n").unwrap();
        let err = load_defaults_at(&path).unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got error: {err}");
    }
}
