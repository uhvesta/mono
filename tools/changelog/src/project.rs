use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ProjectYaml {
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Parse a PROJECT.yaml file and derive the set of owned path globs.
///
/// Semantics: own dir (directory containing PROJECT.yaml, relative to repo root)
/// is implicitly owned. `paths` lists additional owned directories.
/// All directory entries are normalized to `<dir>/**` for globset matching.
pub fn derive_paths_from_project(project_file: &Path, repo_root: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(project_file)
        .with_context(|| format!("could not read {}", project_file.display()))?;
    let project: ProjectYaml = serde_yaml::from_str(&content)
        .with_context(|| format!("invalid PROJECT.yaml: {}", project_file.display()))?;

    let project_canonical = project_file
        .canonicalize()
        .with_context(|| format!("could not canonicalize {}", project_file.display()))?;
    let own_dir_abs = project_canonical
        .parent()
        .ok_or_else(|| anyhow::anyhow!("PROJECT.yaml has no parent directory"))?;
    let own_dir_rel = own_dir_abs.strip_prefix(repo_root).with_context(|| {
        format!(
            "PROJECT.yaml at {} is not under repo root {}",
            project_file.display(),
            repo_root.display()
        )
    })?;

    let mut globs = Vec::new();
    globs.push(dir_to_glob(&own_dir_rel.to_string_lossy()));
    for p in &project.paths {
        globs.push(dir_to_glob(p.trim()));
    }

    Ok(globs)
}

/// Normalize a directory path to a `**` glob pattern.
/// Handles trailing slashes: `tools/cube/` and `tools/cube` both become `tools/cube/**`.
pub fn dir_to_glob(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    format!("{trimmed}/**")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_project_yaml(dir: &Path, content: &str) -> std::path::PathBuf {
        let p = dir.join("PROJECT.yaml");
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn project_with_paths_derives_own_dir_plus_listed() {
        let root = tempdir().unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        let project_dir = root_canonical.join("tools").join("checkleft");
        fs::create_dir_all(&project_dir).unwrap();
        let yaml = "name: checkleft\npaths:\n  - tools/checkleft_package/\n  - tools/checks_js_componentizer/\n";
        let project_file = write_project_yaml(&project_dir, yaml);

        let globs = derive_paths_from_project(&project_file, &root_canonical).unwrap();

        assert_eq!(
            globs,
            vec![
                "tools/checkleft/**".to_string(),
                "tools/checkleft_package/**".to_string(),
                "tools/checks_js_componentizer/**".to_string(),
            ]
        );
    }

    #[test]
    fn project_without_paths_derives_own_dir_only() {
        let root = tempdir().unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        let project_dir = root_canonical.join("tools").join("changelog");
        fs::create_dir_all(&project_dir).unwrap();
        let project_file = write_project_yaml(&project_dir, "name: changelog\n");

        let globs = derive_paths_from_project(&project_file, &root_canonical).unwrap();

        assert_eq!(globs, vec!["tools/changelog/**".to_string()]);
    }

    #[test]
    fn dir_to_glob_strips_trailing_slash() {
        assert_eq!(dir_to_glob("tools/cube/"), "tools/cube/**");
        assert_eq!(dir_to_glob("tools/cube"), "tools/cube/**");
    }

    #[test]
    fn filtering_includes_file_in_additional_path() {
        // Simulate globs derived from checkleft's PROJECT.yaml
        use globset::{Glob, GlobSetBuilder};
        let derived = vec![
            "tools/checkleft/**".to_string(),
            "tools/checkleft_package/**".to_string(),
        ];
        let mut builder = GlobSetBuilder::new();
        for p in &derived {
            builder.add(Glob::new(p).unwrap());
        }
        let gs = builder.build().unwrap();

        assert!(gs.is_match("tools/checkleft_package/src/lib.rs"));
        assert!(gs.is_match("tools/checkleft/src/main.rs"));
    }

    #[test]
    fn filtering_excludes_file_outside_owned_paths() {
        use globset::{Glob, GlobSetBuilder};
        let derived = vec![
            "tools/checkleft/**".to_string(),
            "tools/checkleft_package/**".to_string(),
        ];
        let mut builder = GlobSetBuilder::new();
        for p in &derived {
            builder.add(Glob::new(p).unwrap());
        }
        let gs = builder.build().unwrap();

        assert!(!gs.is_match("tools/changelog/src/main.rs"));
        assert!(!gs.is_match("tools/cube/src/app.rs"));
    }
}
