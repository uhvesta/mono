use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct FrontendNoLegacyApiCheck;

#[async_trait]
impl Check for FrontendNoLegacyApiCheck {
    fn id(&self) -> &str {
        "frontend-no-legacy-api"
    }

    fn description(&self) -> &str {
        "prevents frontend imports from deprecated API modules"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledFrontendNoLegacyApiConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let import_re = Regex::new(r#"^\s*import\b[^;]*\bfrom\s*["']([^"']+)["']"#).expect("valid regex");
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_frontend_source_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                continue;
            };

            for (line_index, line) in contents.lines().enumerate() {
                let Some(captures) = import_re.captures(line) else {
                    continue;
                };
                let Some(module) = captures.get(1).map(|capture| capture.as_str()) else {
                    continue;
                };

                let normalized = normalize_import_module(module);
                let Some(legacy_match) = self
                    .legacy_modules
                    .iter()
                    .find(|legacy| normalized.ends_with(legacy.as_str()))
                else {
                    continue;
                };

                findings.push(Finding {
                    severity: self.severity,
                    message: format!("import from deprecated frontend API module `{legacy_match}`"),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: Some((line_index + 1) as u32),
                        column: Some(1),
                    }),
                    remediations: vec![self.remediation.clone()],
                    suggested_fix: None,
                });
            }
        }

        Ok(CheckResult {
            check_id: "frontend-no-legacy-api".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct FrontendNoLegacyApiConfig {
    #[serde(default)]
    legacy_modules: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledFrontendNoLegacyApiConfig {
    legacy_modules: Vec<String>,
    severity: Severity,
    remediation: String,
}

fn parse_config(config: &toml::Value) -> Result<CompiledFrontendNoLegacyApiConfig> {
    let parsed: FrontendNoLegacyApiConfig = config
        .clone()
        .try_into()
        .context("invalid frontend-no-legacy-api config")?;

    if parsed.legacy_modules.is_empty() {
        bail!("frontend-no-legacy-api config must contain at least one `legacy_modules` entry");
    }

    Ok(CompiledFrontendNoLegacyApiConfig {
        legacy_modules: parsed
            .legacy_modules
            .into_iter()
            .map(|module| module.trim_matches('/').to_owned())
            .collect(),
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
        remediation: parsed
            .remediation
            .unwrap_or_else(|| "Import from supported modules under `frontend/src/api/` instead.".to_owned()),
    })
}

fn is_frontend_source_file(path: &std::path::Path) -> bool {
    if !path.starts_with(std::path::Path::new("frontend/src")) {
        return false;
    }
    matches!(path.extension().and_then(|ext| ext.to_str()), Some("ts") | Some("tsx"))
}

fn normalize_import_module(module: &str) -> String {
    let mut normalized = module.trim_matches('\'').trim_matches('"').to_owned();
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_owned();
    }
    while let Some(stripped) = normalized.strip_prefix("../") {
        normalized = stripped.to_owned();
    }
    normalized.trim_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::FrontendNoLegacyApiCheck;

    #[tokio::test]
    async fn flags_legacy_api_import() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("frontend/src/components")).expect("create dirs");
        fs::write(
            temp.path().join("frontend/src/components/Foo.tsx"),
            "import { x } from \"../api/fencingtracker\";\n",
        )
        .expect("write source");

        let check = FrontendNoLegacyApiCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/components/Foo.tsx").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    legacy_modules = ["api/fencingtracker", "api/usafencing"]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn ignores_non_legacy_imports() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("frontend/src/components")).expect("create dirs");
        fs::write(
            temp.path().join("frontend/src/components/Foo.tsx"),
            "import { getStatusz } from \"../api/statusz\";\n",
        )
        .expect("write source");

        let check = FrontendNoLegacyApiCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("frontend/src/components/Foo.tsx").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    legacy_modules = ["api/fencingtracker", "api/usafencing"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
