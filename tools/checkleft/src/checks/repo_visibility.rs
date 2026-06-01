use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tree_sitter::{Node, Parser};

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct RepoVisibilityCheck;

#[async_trait]
impl Check for RepoVisibilityCheck {
    fn id(&self) -> &str {
        "repo-visibility"
    }

    fn description(&self) -> &str {
        "rejects Bazel packages that default to //visibility:public"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for RepoVisibilityCheck {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_build_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };

            for location in find_public_default_visibility_locations(contents) {
                findings.push(Finding {
                    severity: Severity::Error,
                    message: "package default_visibility must not be `//visibility:public`"
                        .to_owned(),
                    location: Some(Location {
                        path: changed_file.path.clone(),
                        line: Some(location.line),
                        column: Some(location.column),
                    }),
                    remediations: vec![
                        "Remove the package default_visibility or narrow visibility on individual targets."
                            .to_owned(),
                    ],
                    suggested_fix: None,
                });
            }
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

fn is_build_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("BUILD") | Some("BUILD.bazel")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceLocation {
    line: u32,
    column: u32,
}

fn find_public_default_visibility_locations(contents: &str) -> Vec<SourceLocation> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_starlark::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(contents, None) else {
        return Vec::new();
    };
    if tree.root_node().has_error() {
        return Vec::new();
    }

    let mut locations = Vec::new();
    collect_public_default_visibility_locations(
        tree.root_node(),
        contents.as_bytes(),
        &mut locations,
    );
    locations
}

fn collect_public_default_visibility_locations(
    node: Node<'_>,
    source: &[u8],
    locations: &mut Vec<SourceLocation>,
) {
    if let Some(location) = package_public_visibility_location(node, source) {
        locations.push(location);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_public_default_visibility_locations(child, source, locations);
    }
}

fn package_public_visibility_location(node: Node<'_>, source: &[u8]) -> Option<SourceLocation> {
    if node.kind() != "call" || call_function_name(node, source)? != "package" {
        return None;
    }

    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "keyword_argument" {
            continue;
        }
        let Some(name) = argument.child_by_field_name("name") else {
            continue;
        };
        let Ok(name_text) = name.utf8_text(source) else {
            continue;
        };
        if name_text != "default_visibility" {
            continue;
        }

        let value = argument.child_by_field_name("value")?;
        if let Some(location) = find_public_visibility_string(value, source) {
            return Some(location);
        }
    }

    None
}

fn call_function_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let function = node.child_by_field_name("function")?;
    if function.kind() != "identifier" {
        return None;
    }
    function.utf8_text(source).ok()
}

fn find_public_visibility_string(node: Node<'_>, source: &[u8]) -> Option<SourceLocation> {
    if node.kind() == "string" {
        let text = node.utf8_text(source).ok()?;
        if text.contains("//visibility:public") {
            return Some(source_location(node));
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(location) = find_public_visibility_string(child, source) {
            return Some(location);
        }
    }

    None
}

fn source_location(node: Node<'_>) -> SourceLocation {
    let position = node.start_position();
    SourceLocation {
        line: (position.row + 1) as u32,
        column: (position.column + 1) as u32,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::RepoVisibilityCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    #[tokio::test]
    async fn flags_public_package_default_visibility() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(
    default_visibility = [
        "//visibility:public",
    ],
)
"#,
        )
        .expect("write build file");

        let check = RepoVisibilityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0]
                .location
                .as_ref()
                .and_then(|loc| loc.line),
            Some(4)
        );
    }

    #[tokio::test]
    async fn ignores_private_package_default_visibility() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(default_visibility = ["//visibility:private"])
"#,
        )
        .expect("write build file");

        let check = RepoVisibilityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
                    kind: ChangeKind::Modified,
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
    async fn ignores_public_target_visibility() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("BUILD.bazel"),
            r#"
package(default_visibility = ["//visibility:private"])

filegroup(
    name = "example",
    srcs = ["example.txt"],
    visibility = ["//visibility:public"],
)
"#,
        )
        .expect("write build file");

        fs::write(temp.path().join("example.txt"), "example").expect("write source file");

        let check = RepoVisibilityCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("BUILD.bazel").to_path_buf(),
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
