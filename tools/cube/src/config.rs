use std::path::PathBuf;

use serde::Deserialize;

use crate::app::CubeError;

pub fn config_dir() -> Result<PathBuf, CubeError> {
    if let Some(path) = std::env::var_os("CUBE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("cube"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        CubeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set",
        ))
    })?;
    Ok(PathBuf::from(home).join(".config").join("cube"))
}

pub fn config_file_path() -> Result<PathBuf, CubeError> {
    Ok(config_dir()?.join("cube.toml"))
}

/// A user-configured rule that turns a bare `<reponame>` into a clone URL
/// (and optionally a bespoke clone command). Resolvers keep cube ignorant of
/// any particular hosting setup: LinkedIn's `mint`, a corporate GitHub org,
/// etc. all live in the user's config rather than the cube binary.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoResolver {
    /// Human label, surfaced in errors and `cube repo` provenance.
    pub name: String,
    /// Origin URL template. `{name}` is replaced with the resolved
    /// `<reponame>`; the result is recorded as the repo's origin.
    pub origin_pattern: String,
    /// Optional clone command template. When present, the `{name}`-substituted
    /// string is run (in the workspace pool root) in place of `jj git clone`.
    #[serde(default)]
    pub clone_command: Option<String>,
}

impl RepoResolver {
    /// Substitute `{name}` into `origin_pattern`. Returns `None` when the
    /// pattern would yield an empty string (a misconfigured resolver), so the
    /// caller can keep walking the chain.
    pub fn resolve_origin(&self, name: &str) -> Option<String> {
        let url = self.origin_pattern.replace("{name}", name);
        if url.trim().is_empty() {
            None
        } else {
            Some(url)
        }
    }

    /// The `{name}`-substituted clone command, if this resolver declares one.
    pub fn resolve_clone_command(&self, name: &str) -> Option<String> {
        self.clone_command
            .as_ref()
            .map(|cmd| cmd.replace("{name}", name))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CubeConfig {
    /// Ordered list of repo-name resolvers. The first resolver that produces a
    /// URL wins (see `cube repo ensure`).
    #[serde(rename = "repo-resolvers")]
    pub repo_resolvers: Vec<RepoResolver>,
}

/// Load cube user config from the standard config file path.
/// Returns a default (all-off) config if the file does not exist or the home
/// directory cannot be determined.
pub fn load_config() -> Result<CubeConfig, CubeError> {
    let path = match config_file_path() {
        Ok(p) => p,
        // If we can't determine where config lives (e.g. HOME unset), treat it
        // as absent and return defaults rather than propagating a hard error.
        Err(_) => return Ok(CubeConfig::default()),
    };
    if !path.exists() {
        return Ok(CubeConfig::default());
    }
    let content = std::fs::read_to_string(&path).map_err(CubeError::Io)?;
    toml::from_str(&content).map_err(|e| {
        CubeError::InvalidArgument(format!(
            "failed to parse cube config at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_resolvers() {
        let cfg = CubeConfig::default();
        assert!(cfg.repo_resolvers.is_empty());
    }

    #[test]
    fn parse_resolver_with_clone_command() {
        let toml = "[[repo-resolvers]]\n\
            name = \"mint\"\n\
            origin_pattern = \"org-127256988@github.com:linkedin-multiproduct/{name}.git\"\n\
            clone_command = \"mint clone {name}\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 1);
        let r = &cfg.repo_resolvers[0];
        assert_eq!(r.name, "mint");
        assert_eq!(
            r.origin_pattern,
            "org-127256988@github.com:linkedin-multiproduct/{name}.git"
        );
        assert_eq!(r.clone_command.as_deref(), Some("mint clone {name}"));
    }

    #[test]
    fn parse_resolver_without_clone_command() {
        let toml = "[[repo-resolvers]]\n\
            name = \"corp-github\"\n\
            origin_pattern = \"git@github.example.com:corp/{name}.git\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 1);
        assert_eq!(cfg.repo_resolvers[0].clone_command, None);
    }

    #[test]
    fn parse_multiple_resolvers_preserves_order() {
        let toml = "[[repo-resolvers]]\n\
            name = \"first\"\n\
            origin_pattern = \"git@a.example.com:x/{name}.git\"\n\
            [[repo-resolvers]]\n\
            name = \"second\"\n\
            origin_pattern = \"git@b.example.com:y/{name}.git\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 2);
        assert_eq!(cfg.repo_resolvers[0].name, "first");
        assert_eq!(cfg.repo_resolvers[1].name, "second");
    }

    #[test]
    fn resolve_origin_substitutes_name() {
        let cfg: CubeConfig = toml::from_str(
            "[[repo-resolvers]]\n\
             name = \"mint\"\n\
             origin_pattern = \"org-1@github.com:linkedin-multiproduct/{name}.git\"\n\
             clone_command = \"mint clone {name}\"\n",
        )
        .expect("parse");
        let r = &cfg.repo_resolvers[0];
        assert_eq!(
            r.resolve_origin("frontend-api").as_deref(),
            Some("org-1@github.com:linkedin-multiproduct/frontend-api.git")
        );
        assert_eq!(
            r.resolve_clone_command("frontend-api").as_deref(),
            Some("mint clone frontend-api")
        );
    }

    #[test]
    fn load_config_returns_default_when_file_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // CUBE_CONFIG_DIR points to a dir that exists but has no cube.toml
        // SAFETY: test-only; no other threads read this env var concurrently.
        unsafe { std::env::set_var("CUBE_CONFIG_DIR", tmp.path()) };
        let cfg = load_config().expect("load");
        unsafe { std::env::remove_var("CUBE_CONFIG_DIR") };
        assert!(cfg.repo_resolvers.is_empty());
    }
}
