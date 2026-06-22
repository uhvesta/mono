use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::CheckResult;

mod config;
mod java;

use config::{PatternLanguage, parse_config};
use java::analyze_java_file;

#[derive(Debug, Default)]
pub struct CodePatternsCheck;

#[async_trait]
impl Check for CodePatternsCheck {
    fn id(&self) -> &str {
        "code-patterns"
    }

    fn description(&self) -> &str {
        "flags configured language-aware code patterns in changed files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for config::CompiledCodePatternsConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        changeset
            .changed_files
            .iter()
            .filter(|f| !matches!(f.kind, ChangeKind::Deleted) && matches_language_path(&f.path, self.language))
            .count()
    }

    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        self.run_with_progress(changeset, tree, Arc::new(|_| {})).await
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let mut findings = Vec::new();
        let mut processed = 0usize;

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !matches_language_path(&changed_file.path, self.language) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                processed += 1;
                on_file_processed(processed);
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                processed += 1;
                on_file_processed(processed);
                continue;
            };

            findings.extend(analyze_java_file(&changed_file.path, contents, &self.rules));
            processed += 1;
            on_file_processed(processed);
        }

        Ok(CheckResult {
            check_id: "code-patterns".to_owned(),
            findings,
        })
    }
}

fn matches_language_path(path: &Path, language: PatternLanguage) -> bool {
    match language {
        PatternLanguage::Java => {
            matches!(path.extension().and_then(|ext| ext.to_str()), Some("java"))
        }
    }
}

#[cfg(test)]
mod tests;
