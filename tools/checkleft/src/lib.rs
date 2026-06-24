//! Experimental crate under active development.
//!
//! `checkleft` is not yet recommended for general use. The library API, CLI
//! behavior, and built-in checks may change without notice.

pub mod bypass;
pub mod change_detection;
pub mod check;
pub mod checks;
pub mod config;
pub mod exclusion;
pub mod exclusion_matcher;
pub mod external;
pub mod fix;
pub mod input;
pub mod install;
pub mod output;
pub mod path;
pub mod progress;
pub mod runner;
pub mod source_tree;
pub mod vcs;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use async_trait::async_trait;

    use crate::check::{Check, CheckRegistry, ConfiguredCheck};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use crate::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

    struct DummyTree;

    impl SourceTree for DummyTree {
        fn read_file(&self, _path: &Path) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }

        fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn glob(&self, _pattern: &str) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    struct DummyCheck;

    #[async_trait]
    impl Check for DummyCheck {
        fn id(&self) -> &str {
            "dummy"
        }

        fn description(&self) -> &str {
            "dummy check"
        }

        fn configure(&self, _config: &toml::Value) -> Result<std::sync::Arc<dyn ConfiguredCheck>> {
            Ok(std::sync::Arc::new(Self))
        }
    }

    #[async_trait]
    impl ConfiguredCheck for DummyCheck {
        async fn run(&self, _changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
            Ok(CheckResult {
                check_id: self.id().to_owned(),
                findings: Vec::new(),
            })
        }
    }

    #[test]
    fn changeset_roundtrip_json() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("backend/src/main.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let encoded = serde_json::to_string(&changeset).expect("serialize changeset");
        let decoded: ChangeSet = serde_json::from_str(&encoded).expect("deserialize changeset");
        assert_eq!(changeset, decoded);
    }

    #[test]
    fn check_result_roundtrip_json() {
        let result = CheckResult {
            check_id: "spelling-typos".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("docs/conventions.md"),
                    line: Some(3),
                    column: Some(15),
                }),
                remediations: vec!["Use canonical spelling instead".to_owned()],
                suggested_fix: Some(SuggestedFix {
                    description: "Replace typo".to_owned(),
                    edits: vec![FileEdit {
                        path: PathBuf::from("docs/conventions.md"),
                        old_text: "teh".to_owned(),
                        new_text: "the".to_owned(),
                    }],
                }),
            }],
        };

        let encoded = serde_json::to_string(&result).expect("serialize check result");
        let decoded: CheckResult = serde_json::from_str(&encoded).expect("deserialize check result");
        assert_eq!(result, decoded);
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let mut registry = CheckRegistry::new();
        registry.register(DummyCheck).expect("register first check");
        let duplicate = registry.register(DummyCheck);
        assert!(duplicate.is_err());
    }

    #[tokio::test]
    async fn registry_returns_registered_check() {
        let mut registry = CheckRegistry::new();
        registry.register(DummyCheck).expect("register check");
        let check = registry.get("dummy").expect("dummy check exists");
        let result = check
            .run(
                &ChangeSet::default(),
                &DummyTree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");
        assert_eq!(result.check_id, "dummy");
    }
}
