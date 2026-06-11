use std::env;
use std::fs;
use std::path::PathBuf;

use keyring::Entry;
use serde_json::Value;
use thiserror::Error;

const KEYRING_SERVICE: &str = "hood.robinhood.oauth";
const KEYRING_METADATA_SERVICE: &str = "hood.robinhood.meta";
const LAST_USERNAME_KEY: &str = "last_username";
const APP_CONFIG_DIR: &str = "hood";
const LAST_USERNAME_FILE: &str = "last_username";

pub type Result<T> = std::result::Result<T, CredentialsError>;

#[derive(Debug, Error)]
pub enum CredentialsError {
    #[error("failed to open keychain entry for `{username}`")]
    OpenOauthEntry { username: String, source: keyring::Error },
    #[error("failed to open metadata keychain entry for `{key}`")]
    OpenMetadataEntry { key: &'static str, source: keyring::Error },
    #[error("failed to serialize credentials")]
    SerializeCredentials { source: serde_json::Error },
    #[error("failed to write credentials to keychain for `{username}`")]
    WriteCredentials { username: String, source: keyring::Error },
    #[error("failed to read credentials from keychain for `{username}`")]
    ReadCredentials { username: String, source: keyring::Error },
    #[error("failed to parse credentials from keychain for `{username}`")]
    ParseCredentials {
        username: String,
        source: serde_json::Error,
    },
    #[error("failed to read most recent username from keychain")]
    ReadLatestUsername { source: keyring::Error },
    #[error("could not resolve config directory (XDG_CONFIG_HOME or HOME)")]
    MissingConfigDirectory,
    #[error("failed to create config directory `{path}`")]
    CreateConfigDirectory { path: PathBuf, source: std::io::Error },
    #[error("failed to write most recent username file `{path}`")]
    WriteLatestUsernameFile { path: PathBuf, source: std::io::Error },
    #[error("failed to read most recent username file `{path}`")]
    ReadLatestUsernameFile { path: PathBuf, source: std::io::Error },
    #[error("most recent username file `{path}` is empty")]
    EmptyLatestUsernameFile { path: PathBuf },
    #[error("failed to load credentials for `{username}`")]
    LoadNamedCredentials {
        username: String,
        source: Box<CredentialsError>,
    },
    #[error(
        "failed to load credentials for the most recently authenticated user; rerun `hood auth` or pass --username"
    )]
    LoadLatestCredentials { source: Box<CredentialsError> },
    #[error("stored credentials for `{username}` are missing a valid access token")]
    MissingAccessToken { username: String },
}

pub fn store_credentials(username: &str, token: &Value) -> Result<()> {
    let entry = oauth_entry(username)?;
    let serialized =
        serde_json::to_string(token).map_err(|source| CredentialsError::SerializeCredentials { source })?;

    entry
        .set_password(&serialized)
        .map_err(|source| CredentialsError::WriteCredentials {
            username: username.to_string(),
            source,
        })?;

    store_last_username(username)?;
    Ok(())
}

pub fn load_credentials(username: &str) -> Result<Value> {
    let entry = oauth_entry(username)?;
    let raw = entry
        .get_password()
        .map_err(|source| CredentialsError::ReadCredentials {
            username: username.to_string(),
            source,
        })?;

    serde_json::from_str(&raw).map_err(|source| CredentialsError::ParseCredentials {
        username: username.to_string(),
        source,
    })
}

pub fn load_latest_credentials() -> Result<(String, Value)> {
    let username = load_last_username()?;
    let credentials = load_credentials(&username)?;
    Ok((username, credentials))
}

pub fn load_access_token(username: Option<&str>) -> Result<(String, String)> {
    let (username, credentials) = match username {
        Some(username) => load_credentials(username)
            .map(|credentials| (username.to_string(), credentials))
            .map_err(|source| CredentialsError::LoadNamedCredentials {
                username: username.to_string(),
                source: Box::new(source),
            })?,
        None => load_latest_credentials().map_err(|source| CredentialsError::LoadLatestCredentials {
            source: Box::new(source),
        })?,
    };

    let access_token = extract_access_token(&credentials)
        .ok_or_else(|| CredentialsError::MissingAccessToken {
            username: username.clone(),
        })?
        .to_string();

    Ok((username, access_token))
}

fn oauth_entry(username: &str) -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, username).map_err(|source| CredentialsError::OpenOauthEntry {
        username: username.to_string(),
        source,
    })
}

fn metadata_entry(key: &'static str) -> Result<Entry> {
    Entry::new(KEYRING_METADATA_SERVICE, key).map_err(|source| CredentialsError::OpenMetadataEntry { key, source })
}

fn store_last_username(username: &str) -> Result<()> {
    store_last_username_in_file(username)?;

    // Keep writing metadata to keychain for backward compatibility with
    // older clients that only read this entry.
    if let Ok(entry) = metadata_entry(LAST_USERNAME_KEY) {
        let _ = entry.set_password(username);
    }

    Ok(())
}

fn load_last_username() -> Result<String> {
    if let Ok(username) = load_last_username_from_file() {
        return Ok(username);
    }

    metadata_entry(LAST_USERNAME_KEY)?
        .get_password()
        .map_err(|source| CredentialsError::ReadLatestUsername { source })
}

fn store_last_username_in_file(username: &str) -> Result<()> {
    let path = latest_username_file_path()?;
    let parent = path
        .parent()
        .map(PathBuf::from)
        .ok_or(CredentialsError::MissingConfigDirectory)?;

    fs::create_dir_all(&parent).map_err(|source| CredentialsError::CreateConfigDirectory { path: parent, source })?;

    fs::write(&path, username).map_err(|source| CredentialsError::WriteLatestUsernameFile { path, source })
}

fn load_last_username_from_file() -> Result<String> {
    let path = latest_username_file_path()?;
    let username = fs::read_to_string(&path).map_err(|source| CredentialsError::ReadLatestUsernameFile {
        path: path.clone(),
        source,
    })?;
    let username = username.trim();

    if username.is_empty() {
        return Err(CredentialsError::EmptyLatestUsernameFile { path });
    }

    Ok(username.to_string())
}

fn latest_username_file_path() -> Result<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join(APP_CONFIG_DIR).join(LAST_USERNAME_FILE));
    }

    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join(APP_CONFIG_DIR)
            .join(LAST_USERNAME_FILE));
    }

    Err(CredentialsError::MissingConfigDirectory)
}

fn extract_access_token(credentials: &Value) -> Option<&str> {
    credentials
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_access_token;

    #[test]
    fn extract_access_token_returns_none_when_missing() {
        let credentials = json!({});
        assert_eq!(extract_access_token(&credentials), None);
    }

    #[test]
    fn extract_access_token_returns_none_when_blank() {
        let credentials = json!({ "access_token": "   " });
        assert_eq!(extract_access_token(&credentials), None);
    }

    #[test]
    fn extract_access_token_returns_trimmed_token() {
        let credentials = json!({ "access_token": "  abc123  " });
        assert_eq!(extract_access_token(&credentials), Some("abc123"));
    }
}
