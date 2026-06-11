use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct RustTestRuleCoverageCheck;

#[async_trait]
impl Check for RustTestRuleCoverageCheck {
    fn id(&self) -> &str {
        "rust-test-rule-coverage"
    }

    fn description(&self) -> &str {
        "requires new Rust test files to live in packages with a Bazel rust_test rule"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for RustTestRuleCoverageCheck {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if !matches!(changed_file.kind, ChangeKind::Added) {
                continue;
            }
            if !is_rust_source_file(&changed_file.path) {
                continue;
            }
            if !looks_like_test_file(&changed_file.path, tree) {
                continue;
            }
            if package_has_rust_test_rule(&changed_file.path, tree) {
                continue;
            }

            findings.push(Finding {
                severity: Severity::Error,
                message: "new Rust test file is not covered by a package rust_test rule".to_owned(),
                location: Some(Location {
                    path: changed_file.path.clone(),
                    line: None,
                    column: None,
                }),
                remediations: vec![
                    "Add a Bazel `rust_test(...)` target in the nearest BUILD/BUILD.bazel package.".to_owned(),
                ],
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

fn is_rust_source_file(path: &Path) -> bool {
    matches!(path.extension().and_then(|ext| ext.to_str()), Some("rs"))
}

fn looks_like_test_file(path: &Path, tree: &dyn SourceTree) -> bool {
    if path.components().any(|component| component.as_os_str() == "tests") {
        return true;
    }
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("_test.rs"))
    {
        return true;
    }

    let Ok(contents) = tree.read_file(path) else {
        return false;
    };
    let Ok(contents) = String::from_utf8(contents) else {
        return false;
    };

    contents.contains("#[test]") || contents.contains("#[tokio::test]")
}

fn package_has_rust_test_rule(file_path: &Path, tree: &dyn SourceTree) -> bool {
    for dir in ancestor_dirs(file_path) {
        for build_file in ["BUILD.bazel", "BUILD"] {
            let candidate = if dir.as_os_str().is_empty() {
                PathBuf::from(build_file)
            } else {
                dir.join(build_file)
            };
            if !tree.exists(&candidate) {
                continue;
            }
            let Ok(contents) = tree.read_file(&candidate) else {
                continue;
            };
            let Ok(contents) = String::from_utf8(contents) else {
                continue;
            };
            if contents.contains("rust_test(") {
                return true;
            }
        }
    }

    false
}

fn ancestor_dirs(path: &Path) -> Vec<PathBuf> {
    let mut output = Vec::new();
    let mut current = path.parent();
    while let Some(dir) = current {
        output.push(dir.to_path_buf());
        current = dir.parent();
    }
    output.push(PathBuf::new());
    output
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::RustTestRuleCoverageCheck;

    #[tokio::test]
    async fn flags_new_test_file_without_rust_test_rule() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/foo/tests")).expect("create dirs");
        fs::write(
            temp.path().join("backend/foo/tests/new_test.rs"),
            "#[test]\nfn it_works() {}\n",
        )
        .expect("write test");
        fs::write(temp.path().join("backend/foo/BUILD"), "rust_library(name = \"foo\")\n").expect("write build");

        let check = RustTestRuleCoverageCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/foo/tests/new_test.rs").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn accepts_new_test_file_when_rust_test_rule_exists() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/foo/tests")).expect("create dirs");
        fs::write(
            temp.path().join("backend/foo/tests/new_test.rs"),
            "#[test]\nfn it_works() {}\n",
        )
        .expect("write test");
        fs::write(
            temp.path().join("backend/foo/BUILD"),
            "rust_library(name = \"foo\")\nrust_test(name = \"foo_test\", crate = \":foo\")\n",
        )
        .expect("write build");

        let check = RustTestRuleCoverageCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/foo/tests/new_test.rs").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_non_test_rust_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/foo/src")).expect("create dirs");
        fs::write(
            temp.path().join("backend/foo/src/lib.rs"),
            "pub fn value() -> i32 { 1 }\n",
        )
        .expect("write source");

        let check = RustTestRuleCoverageCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/foo/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
