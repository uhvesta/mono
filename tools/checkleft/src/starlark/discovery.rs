use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::input::{ChangeSet, SourceTree};
use crate::path::validate_relative_path;
use crate::starlark::manifest::{PackageKind, PackageManifest};

const BUILTIN_ADAPTERS: &[&str] = &["text", "proto", "module_json", "java"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCheck {
    pub id: String,
    pub adapter: String,
    pub checkleft_root: PathBuf,
    pub scope_root: PathBuf,
    pub activation_include_patterns: Vec<String>,
    pub activation_exclude_patterns: Vec<String>,
    pub check_dir: PathBuf,
    pub check_path: PathBuf,
    pub fix_path: Option<PathBuf>,
    pub check_meta: DiscoveredCheckMeta,
    pub package: PackageManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCheckMeta {
    pub applies_to: Vec<String>,
}

pub fn discover_local_checks(changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<Vec<DiscoveredCheck>> {
    let roots = candidate_checkleft_roots(changeset, tree)?;
    let mut checks = Vec::new();
    for root in roots {
        checks.extend(discover_package_checks(tree, &root)?);
    }
    checks.sort_by(|a, b| a.check_path.cmp(&b.check_path));
    Ok(checks)
}

pub fn discover_package_checks(tree: &dyn SourceTree, checkleft_root: &Path) -> Result<Vec<DiscoveredCheck>> {
    validate_relative_path(checkleft_root)?;
    let manifest = PackageManifest::read_from_tree(tree, checkleft_root)?;
    if manifest.package.kind == PackageKind::VersionSet {
        ensure_version_set_defines_no_checks(tree, checkleft_root)?;
        return Ok(Vec::new());
    }
    let mut checks = Vec::new();
    scan_dir(tree, checkleft_root, checkleft_root, &manifest, &mut checks)?;
    checks.sort_by(|a, b| a.check_path.cmp(&b.check_path));
    Ok(checks)
}

fn ensure_version_set_defines_no_checks(tree: &dyn SourceTree, checkleft_root: &Path) -> Result<()> {
    for entry in tree
        .list_dir(checkleft_root)
        .with_context(|| format!("failed to list {}", checkleft_root.display()))?
    {
        let relative = relative_to_root(checkleft_root, &entry)?;
        let components = path_components(&relative)?;
        if components.first() == Some(&"lib") {
            bail!(
                "version_set package {} must not define lib/ helpers",
                checkleft_root.display()
            );
        }
        if components
            .first()
            .is_some_and(|component| *component != "package.toml" && is_known_adapter(component))
        {
            bail!(
                "version_set package {} must not define checks; use [includes.<name>] only",
                checkleft_root.display()
            );
        }
        if is_directory(tree, &entry) {
            validate_directory_entry(checkleft_root, &entry)?;
        } else {
            validate_file_entry(checkleft_root, &entry)?;
        }
    }
    Ok(())
}

fn candidate_checkleft_roots(changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    for changed in &changeset.changed_files {
        validate_relative_path(&changed.path)?;
        let mut dir = changed.path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        loop {
            let checkleft_root = if dir.as_os_str().is_empty() {
                PathBuf::from("checkleft")
            } else {
                dir.join("checkleft")
            };
            if tree.exists(&checkleft_root.join("package.toml")) {
                roots.insert(checkleft_root);
            }
            if dir.as_os_str().is_empty() {
                break;
            }
            dir = dir.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        }
    }
    Ok(roots.into_iter().collect())
}

fn scan_dir(
    tree: &dyn SourceTree,
    checkleft_root: &Path,
    dir: &Path,
    manifest: &PackageManifest,
    checks: &mut Vec<DiscoveredCheck>,
) -> Result<()> {
    for entry in tree
        .list_dir(dir)
        .with_context(|| format!("failed to list {}", dir.display()))?
    {
        if is_directory(tree, &entry) {
            validate_directory_entry(checkleft_root, &entry)?;
            scan_dir(tree, checkleft_root, &entry, manifest, checks)?;
            continue;
        }

        validate_file_entry(checkleft_root, &entry)?;
        if entry.file_name().and_then(|name| name.to_str()) == Some("check.checkleft") {
            checks.push(parse_check_file(tree, checkleft_root, &entry, manifest)?);
        }
    }
    Ok(())
}

fn is_directory(tree: &dyn SourceTree, path: &Path) -> bool {
    tree.list_dir(path).is_ok()
}

fn validate_directory_entry(checkleft_root: &Path, entry: &Path) -> Result<()> {
    let relative = relative_to_root(checkleft_root, entry)?;
    let components = path_components(&relative)?;
    if components.is_empty() {
        return Ok(());
    }
    if components[0] == "lib" || components[0] == "testdata" {
        return Ok(());
    }
    if components.len() == 1 {
        if !is_known_adapter(components[0]) {
            bail!("unknown Starlark check adapter directory `{}`", components[0]);
        }
        return Ok(());
    }
    Ok(())
}

fn validate_file_entry(checkleft_root: &Path, entry: &Path) -> Result<()> {
    let relative = relative_to_root(checkleft_root, entry)?;
    let components = path_components(&relative)?;
    if components == ["package.toml"] {
        return Ok(());
    }
    if components.first() == Some(&"lib") {
        if entry.extension().and_then(|ext| ext.to_str()) != Some("checkleft") {
            bail!("Starlark helper files must use .checkleft: {}", entry.display());
        }
        return Ok(());
    }
    if matches!(
        entry.extension().and_then(|ext| ext.to_str()),
        Some("star" | "bzl" | "py")
    ) {
        bail!("Starlark check files must use .checkleft, not {}", entry.display());
    }
    Ok(())
}

fn parse_check_file(
    tree: &dyn SourceTree,
    checkleft_root: &Path,
    check_path: &Path,
    manifest: &PackageManifest,
) -> Result<DiscoveredCheck> {
    let relative = relative_to_root(checkleft_root, check_path)?;
    let components = path_components(&relative)?;
    if components.len() < 3 {
        bail!(
            "{} must be under <adapter>/<name>/check.checkleft",
            check_path.display()
        );
    }
    if components.last() != Some(&"check.checkleft") {
        bail!("internal error: expected check.checkleft, got {}", check_path.display());
    }

    let adapter = components[0].to_owned();
    if !is_known_adapter(&adapter) {
        bail!("unknown Starlark check adapter `{adapter}`");
    }
    let name_components = &components[1..components.len() - 1];
    if name_components.is_empty() {
        bail!("{} must include a check name directory", check_path.display());
    }
    let check_name = name_components.join("/");
    let check_dir = check_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", check_path.display()))?
        .to_path_buf();
    let source = String::from_utf8(
        tree.read_file(check_path)
            .with_context(|| format!("failed to read {}", check_path.display()))?,
    )
    .with_context(|| format!("{} is not valid UTF-8", check_path.display()))?;

    Ok(DiscoveredCheck {
        id: format!("{adapter}/{check_name}"),
        adapter,
        checkleft_root: checkleft_root.to_path_buf(),
        scope_root: default_scope_root(checkleft_root),
        activation_include_patterns: vec!["**".to_owned()],
        activation_exclude_patterns: Vec::new(),
        fix_path: tree
            .exists(&check_dir.join("fix.checkleft"))
            .then(|| check_dir.join("fix.checkleft")),
        check_dir,
        check_path: check_path.to_path_buf(),
        check_meta: DiscoveredCheckMeta {
            applies_to: parse_applies_to(&source)
                .with_context(|| format!("failed to parse check_meta() in {}", check_path.display()))?,
        },
        package: manifest.clone(),
    })
}

fn parse_applies_to(source: &str) -> Result<Vec<String>> {
    let capture = source
        .split("check_meta(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .ok_or_else(|| anyhow!("check_meta(...) is required"))?;

    let applies_to_raw = capture
        .split("applies_to")
        .nth(1)
        .and_then(|rest| rest.split('[').nth(1))
        .and_then(|rest| rest.split(']').next())
        .ok_or_else(|| anyhow!("check_meta() must set applies_to = [...]"))?;

    let applies_to = applies_to_raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.trim_matches('"').trim_matches('\'').to_owned())
        .collect::<Vec<_>>();
    if applies_to.is_empty() {
        bail!("check_meta.applies_to must contain at least one glob");
    }
    Ok(applies_to)
}

fn is_known_adapter(adapter: &str) -> bool {
    BUILTIN_ADAPTERS.contains(&adapter)
}

fn default_scope_root(checkleft_root: &Path) -> PathBuf {
    checkleft_root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn relative_to_root(root: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))
}

fn path_components(path: &Path) -> Result<Vec<&str>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(
                part.to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 path component in {}", path.display()))?,
            ),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("invalid path under checkleft root: {}", path.display());
            }
        }
    }
    Ok(components)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::input::{ChangeKind, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    #[test]
    fn discovers_ancestor_and_nested_local_checks() {
        let temp = tempdir().expect("create temp dir");
        write_file(
            temp.path().join("checkleft/package.toml"),
            r#"
[package]
name = "myorg/root"
version = "0.1.0"
"#,
        );
        write_file(
            temp.path().join("checkleft/text/root_policy/check.checkleft"),
            r#"check_meta(applies_to = ["**/*.txt"])"#,
        );
        write_file(
            temp.path().join("services/payments/checkleft/package.toml"),
            r#"
[package]
name = "myorg/payments"
version = "0.1.0"
"#,
        );
        write_file(
            temp.path()
                .join("services/payments/checkleft/text/team/policy/check.checkleft"),
            r#"check_meta(applies_to = ["**/*.txt"])"#,
        );
        write_file(
            temp.path()
                .join("services/payments/checkleft/text/team/policy/fix.checkleft"),
            "def fix(ctx, findings): return []",
        );
        write_file(temp.path().join("services/payments/readme.txt"), "hello");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("services/payments/readme.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let checks = discover_local_checks(&changeset, &tree).expect("discover checks");
        let ids = checks.iter().map(|check| check.id.as_str()).collect::<Vec<_>>();

        assert_eq!(ids, vec!["text/root_policy", "text/team/policy"]);
        assert_eq!(
            checks[1].fix_path.as_deref(),
            Some(Path::new("services/payments/checkleft/text/team/policy/fix.checkleft"))
        );
        assert_eq!(checks[1].package.package.name, "myorg/payments");
    }

    #[test]
    fn ignores_checkleft_directory_without_package_manifest() {
        let temp = tempdir().expect("create temp dir");
        write_file(
            temp.path().join("checkleft/text/root_policy/check.checkleft"),
            r#"check_meta(applies_to = ["**/*.txt"])"#,
        );
        write_file(temp.path().join("readme.txt"), "hello");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("readme.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let checks = discover_local_checks(&changeset, &tree).expect("discover checks");
        assert!(checks.is_empty());
    }

    #[test]
    fn rejects_unknown_adapter_directory() {
        let temp = tempdir().expect("create temp dir");
        write_file(
            temp.path().join("checkleft/package.toml"),
            r#"
[package]
name = "myorg/root"
version = "0.1.0"
"#,
        );
        write_file(
            temp.path().join("checkleft/unknown/root_policy/check.checkleft"),
            r#"check_meta(applies_to = ["**/*.txt"])"#,
        );

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let err = discover_package_checks(&tree, Path::new("checkleft")).expect_err("adapter must fail");

        assert!(err.to_string().contains("unknown Starlark check adapter"), "{err:#}");
    }

    #[test]
    fn version_set_package_does_not_discover_checks() {
        let temp = tempdir().expect("create temp dir");
        write_file(
            temp.path().join("checkleft/package.toml"),
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.core]
source = "registry://checkleft-hub/core"
version = "1.2.3"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        );

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let checks = discover_package_checks(&tree, Path::new("checkleft")).expect("discover version set");

        assert!(checks.is_empty());
    }

    #[test]
    fn rejects_version_set_package_with_checks() {
        let temp = tempdir().expect("create temp dir");
        write_file(
            temp.path().join("checkleft/package.toml"),
            r#"
[package]
name = "myorg/baseline"
version = "2026.06.1"
kind = "version_set"

[includes.core]
source = "registry://checkleft-hub/core"
version = "1.2.3"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        );
        write_file(
            temp.path().join("checkleft/text/no_debug/check.checkleft"),
            r#"check_meta(applies_to = ["**/*.txt"])"#,
        );

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let err = discover_package_checks(&tree, Path::new("checkleft")).expect_err("version set checks must fail");

        assert!(err.to_string().contains("must not define checks"), "{err:#}");
    }

    fn write_file(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
    }
}
