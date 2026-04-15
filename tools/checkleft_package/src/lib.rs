use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use tar::{Builder, HeaderMode};
use toml::Value;
use toml::map::Map;
use walkdir::WalkDir;

const CHECKLEFT_SUBDIR: &str = "tools/checkleft";
const RULES_RUST_VERSION: &str = "0.68.1";
const ASPECT_RULES_JS_VERSION: &str = "2.8.2";
const COMPATIBILITY_LEVEL: u32 = 0;

pub fn find_checkleft_repo_root(start: &Path) -> Result<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.join(CHECKLEFT_SUBDIR).join("Cargo.toml").is_file()
            && ancestor.join("Cargo.toml").is_file()
        {
            return Ok(ancestor.to_path_buf());
        }
    }

    bail!(
        "could not find repo root from `{}`; expected `{CHECKLEFT_SUBDIR}/Cargo.toml` in an ancestor",
        start.display()
    );
}

pub fn package_checkleft_source_archive(
    repo_root: &Path,
    output: Option<PathBuf>,
) -> Result<PathBuf> {
    let package_root = repo_root.join(CHECKLEFT_SUBDIR);
    if !package_root.is_dir() {
        bail!("expected checkleft sources at `{}`", package_root.display());
    }

    let package_manifest =
        fs::read_to_string(package_root.join("Cargo.toml")).with_context(|| {
            format!(
                "failed to read `{}`",
                package_root.join("Cargo.toml").display()
            )
        })?;
    let workspace_manifest =
        fs::read_to_string(repo_root.join("Cargo.toml")).with_context(|| {
            format!(
                "failed to read `{}`",
                repo_root.join("Cargo.toml").display()
            )
        })?;

    let flattened_manifest = flatten_checkleft_manifest(&package_manifest, &workspace_manifest)
        .context("failed to flatten Cargo manifest")?;
    let version = package_version(&flattened_manifest)?;
    let package_dir_name = format!("checkleft-{version}-source");
    let output = output.unwrap_or_else(|| {
        repo_root
            .join("dist")
            .join(format!("{package_dir_name}.tgz"))
    });
    let output = if output.is_absolute() {
        output
    } else {
        repo_root.join(output)
    };

    let staging_parent = output
        .parent()
        .context("output path must have a parent directory")?;
    fs::create_dir_all(staging_parent)
        .with_context(|| format!("failed to create `{}`", staging_parent.display()))?;

    let staging_root =
        staging_parent.join(format!(".{package_dir_name}.stage-{}", std::process::id()));
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)
            .with_context(|| format!("failed to clear `{}`", staging_root.display()))?;
    }
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("failed to create `{}`", staging_root.display()))?;
    let package_output_root = staging_root.join(&package_dir_name);
    fs::create_dir_all(&package_output_root)
        .with_context(|| format!("failed to create `{}`", package_output_root.display()))?;

    let stage_result = stage_checkleft_source_tree(
        repo_root,
        &package_root,
        &package_output_root,
        &flattened_manifest,
    );
    let archive_result =
        stage_result.and_then(|()| create_source_archive(&package_output_root, &output));
    let cleanup_result = fs::remove_dir_all(&staging_root)
        .with_context(|| format!("failed to clean `{}`", staging_root.display()));

    match (archive_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(output),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

fn stage_checkleft_source_tree(
    repo_root: &Path,
    package_root: &Path,
    output_root: &Path,
    flattened_manifest: &str,
) -> Result<()> {
    write_string(output_root.join("Cargo.toml"), flattened_manifest)?;
    copy_file(repo_root.join("Cargo.lock"), output_root.join("Cargo.lock"))?;
    copy_if_exists(
        repo_root.join(".bazelversion"),
        output_root.join(".bazelversion"),
    )?;
    copy_file(
        package_root.join("README.md"),
        output_root.join("README.md"),
    )?;
    copy_file(package_root.join("LICENSE"), output_root.join("LICENSE"))?;
    copy_tree(package_root.join("src"), output_root.join("src"))?;
    copy_tree(package_root.join("api"), output_root.join("api"))?;
    fs::create_dir_all(output_root.join("bazel"))
        .with_context(|| format!("failed to create `{}`", output_root.join("bazel").display()))?;
    write_string(
        output_root.join("bazel/defs.bzl"),
        &render_release_bazel_defs(repo_root)?,
    )?;
    write_string(
        output_root.join("bazel/BUILD.bazel"),
        &render_bazel_package_build(),
    )?;
    write_string(output_root.join("BUILD.bazel"), &render_build_bazel())?;
    write_string(
        output_root.join("MODULE.bazel"),
        &render_module_bazel(flattened_manifest)?,
    )?;
    write_string(
        output_root.join("BAZEL_CONSUMPTION.md"),
        &render_bazel_consumption_doc(flattened_manifest)?,
    )?;
    Ok(())
}

fn create_source_archive(package_output_root: &Path, output: &Path) -> Result<()> {
    let archive_file = fs::File::create(output)
        .with_context(|| format!("failed to create `{}`", output.display()))?;
    let encoder = GzEncoder::new(archive_file, Compression::default());
    let mut builder = Builder::new(encoder);
    builder.mode(HeaderMode::Deterministic);

    for entry in WalkDir::new(package_output_root).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        if path == package_output_root {
            continue;
        }

        let relative = path
            .strip_prefix(
                package_output_root
                    .parent()
                    .context("packaged root must have a parent directory")?,
            )
            .context("failed to compute archive path")?;

        if entry.file_type().is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_mtime(0);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, relative, io::empty())
                .with_context(|| format!("failed to append `{}`", relative.display()))?;
            continue;
        }

        let mut file =
            fs::File::open(path).with_context(|| format!("failed to read `{}`", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to stat `{}`", path.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&metadata);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, relative, &mut file)
            .with_context(|| format!("failed to append `{}`", relative.display()))?;
    }

    builder.finish().context("failed to finish tar archive")?;
    Ok(())
}

fn render_build_bazel() -> String {
    r#"load("@checkleft_crates//:defs.bzl", "all_crate_deps")
load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_library", "rust_test")

package(default_visibility = ["//visibility:private"])

exports_files([
    ".bazelversion",
    "BAZEL_CONSUMPTION.md",
    "Cargo.toml",
    "Cargo.lock",
    "LICENSE",
    "README.md",
])

rust_library(
    name = "checkleft_lib",
    crate_name = "checkleft",
    srcs = glob(
        ["src/**/*.rs"],
        exclude = ["src/main.rs"],
    ),
    crate_root = "src/lib.rs",
    edition = "2024",
    visibility = ["//visibility:public"],
    deps = all_crate_deps(normal = True),
    proc_macro_deps = all_crate_deps(proc_macro = True),
)

rust_binary(
    name = "checkleft",
    srcs = glob(["src/**/*.rs"]),
    crate_root = "src/main.rs",
    edition = "2024",
    visibility = ["//visibility:public"],
    deps = [
        ":checkleft_lib",
    ] + all_crate_deps(normal = True),
    proc_macro_deps = all_crate_deps(proc_macro = True),
)

rust_test(
    name = "checkleft_lib_test",
    crate = ":checkleft_lib",
    deps = all_crate_deps(
        normal = True,
        normal_dev = True,
    ),
    proc_macro_deps = all_crate_deps(
        proc_macro = True,
        proc_macro_dev = True,
    ),
)

rust_test(
    name = "checkleft_bin_test",
    crate = ":checkleft",
    deps = all_crate_deps(
        normal = True,
        normal_dev = True,
    ),
    proc_macro_deps = all_crate_deps(
        proc_macro = True,
        proc_macro_dev = True,
    ),
)
"#
    .to_owned()
}

fn render_bazel_package_build() -> String {
    r#"package(default_visibility = ["//visibility:private"])

exports_files(["defs.bzl"])
"#
    .to_owned()
}

fn render_release_bazel_defs(repo_root: &Path) -> Result<String> {
    let source = fs::read_to_string(repo_root.join(CHECKLEFT_SUBDIR).join("bazel/defs.bzl"))
        .with_context(|| {
            format!(
                "failed to read `{}`",
                repo_root
                    .join(CHECKLEFT_SUBDIR)
                    .join("bazel/defs.bzl")
                    .display()
            )
        })?;

    let needle = "default = \"//tools/checkleft:checkleft\",";
    let replacement = "default = \"//:checkleft\",";
    if !source.contains(needle) {
        bail!("expected `{needle}` in Bazel defs template");
    }

    Ok(source.replacen(needle, replacement, 1))
}

fn render_module_bazel(flattened_manifest: &str) -> Result<String> {
    let manifest = parse_toml(flattened_manifest)?;
    let version = package_version(flattened_manifest)?;
    let edition = package_field_string(&manifest, "edition")?;
    let rust_version = package_field_string(&manifest, "rust-version")?;

    Ok(format!(
        r#"# Root-module setup for standalone builds of this archive.
# When this archive is consumed via `bazel_dep`, the consuming root must
# register a compatible Rust toolchain itself. See BAZEL_CONSUMPTION.md.

module(
    name = "checkleft",
    version = "{version}",
    compatibility_level = {COMPATIBILITY_LEVEL},
)

bazel_dep(name = "rules_rust", version = "{RULES_RUST_VERSION}")
bazel_dep(name = "aspect_rules_js", version = "{ASPECT_RULES_JS_VERSION}")

rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")
rust.toolchain(
    edition = "{edition}",
    rustfmt_version = "{rust_version}",
    versions = ["{rust_version}"],
)
use_repo(rust, "rust_toolchains")
register_toolchains("@rust_toolchains//:all")

crate = use_extension("@rules_rust//crate_universe:extensions.bzl", "crate")
crate.from_cargo(
    name = "checkleft_crates",
    cargo_lockfile = "//:Cargo.lock",
    manifests = ["//:Cargo.toml"],
)
use_repo(crate, "checkleft_crates")
"#
    ))
}

fn render_bazel_consumption_doc(flattened_manifest: &str) -> Result<String> {
    let version = package_version(flattened_manifest)?;
    let edition = {
        let manifest = parse_toml(flattened_manifest)?;
        package_field_string(&manifest, "edition")?
    };
    let rust_version = {
        let manifest = parse_toml(flattened_manifest)?;
        package_field_string(&manifest, "rust-version")?
    };

    Ok(format!(
        r#"# Consuming `checkleft` with Bzlmod

This archive can be built directly as a standalone Bazel root module.

When `checkleft` is consumed from another repo via `bazel_dep`, Bazel does not
apply this archive's `register_toolchains(...)` call to the consuming root
module. The consuming root must register a compatible Rust toolchain itself.

`checkleft {version}` expects Rust `{rust_version}`.

If your root repo already registers a compatible `rules_rust` toolchain, you
can keep using that and skip the toolchain block below.

Example `MODULE.bazel` for a consuming root:

```starlark
module(name = "your_repo")

bazel_dep(name = "rules_rust", version = "{RULES_RUST_VERSION}")

rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")
rust.toolchain(
    edition = "{edition}",
    rustfmt_version = "{rust_version}",
    versions = ["{rust_version}"],
)
use_repo(rust, "rust_toolchains")
register_toolchains("@rust_toolchains//:all")

bazel_dep(name = "checkleft", version = "{version}")
archive_override(
    module_name = "checkleft",
    urls = ["https://example.invalid/checkleft-{version}-source.tgz"],
    strip_prefix = "checkleft-{version}-source",
)
```

During local development, replace `archive_override(...)` with:

```starlark
local_path_override(
    module_name = "checkleft",
    path = "/absolute/path/to/checkleft-{version}-source",
)
```

Public Bazel surface exported by the archive:

- `@checkleft//:checkleft` - the CLI binary
- `@checkleft//bazel:defs.bzl` - `local_check`, `check_index`, and `checkleft`
- `@checkleft//api:checkleft_exec_pkg` - JS helper package for `@checkleft/exec`
"#
    ))
}

fn flatten_checkleft_manifest(package_manifest: &str, workspace_manifest: &str) -> Result<String> {
    let mut package = parse_toml(package_manifest)?;
    let workspace = parse_toml(workspace_manifest)?;

    let workspace_table = workspace
        .get("workspace")
        .and_then(Value::as_table)
        .context("workspace manifest is missing [workspace]")?;
    let workspace_resolver = workspace_table
        .get("resolver")
        .and_then(Value::as_str)
        .unwrap_or("2")
        .to_owned();
    let workspace_package = workspace_table
        .get("package")
        .and_then(Value::as_table)
        .context("workspace manifest is missing [workspace.package]")?;
    let workspace_dependencies = workspace_table
        .get("dependencies")
        .and_then(Value::as_table)
        .context("workspace manifest is missing [workspace.dependencies]")?;

    package.remove("workspace");

    let package_table = package
        .get_mut("package")
        .and_then(Value::as_table_mut)
        .context("package manifest is missing [package]")?;
    package_table.remove("workspace");
    resolve_workspace_package_fields(package_table, workspace_package)?;
    resolve_workspace_dependencies_in_table(&mut package, workspace_dependencies)?;
    package.insert(
        "workspace".to_owned(),
        Value::Table(Map::from_iter([
            (
                "members".to_owned(),
                Value::Array(vec![Value::String(".".to_owned())]),
            ),
            ("resolver".to_owned(), Value::String(workspace_resolver)),
        ])),
    );

    toml::to_string_pretty(&Value::Table(package)).context("failed to serialize flattened manifest")
}

fn resolve_workspace_package_fields(
    package_table: &mut Map<String, Value>,
    workspace_package: &Map<String, Value>,
) -> Result<()> {
    let keys: Vec<String> = package_table.keys().cloned().collect();
    for key in keys {
        let Some(value) = package_table.get(&key) else {
            continue;
        };
        let inherits_workspace = value
            .as_table()
            .and_then(|table| table.get("workspace"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !inherits_workspace {
            continue;
        }
        let replacement = workspace_package
            .get(&key)
            .cloned()
            .with_context(|| format!("missing `[workspace.package].{key}` in root manifest"))?;
        package_table.insert(key, replacement);
    }
    Ok(())
}

fn resolve_workspace_dependencies_in_table(
    table: &mut Map<String, Value>,
    workspace_dependencies: &Map<String, Value>,
) -> Result<()> {
    let keys: Vec<String> = table.keys().cloned().collect();
    for key in keys {
        let Some(value) = table.get_mut(&key) else {
            continue;
        };

        if is_dependency_table_name(&key) {
            let dependency_table = value
                .as_table_mut()
                .with_context(|| format!("expected `{key}` to be a TOML table"))?;
            let dependency_names: Vec<String> = dependency_table.keys().cloned().collect();
            for dependency_name in dependency_names {
                let dependency_value = dependency_table
                    .get(&dependency_name)
                    .cloned()
                    .with_context(|| format!("missing dependency `{dependency_name}`"))?;
                let resolved = resolve_workspace_dependency(
                    &dependency_name,
                    dependency_value,
                    workspace_dependencies,
                )?;
                dependency_table.insert(dependency_name, resolved);
            }
            continue;
        }

        if let Some(child) = value.as_table_mut() {
            resolve_workspace_dependencies_in_table(child, workspace_dependencies)?;
        }
    }
    Ok(())
}

fn resolve_workspace_dependency(
    dependency_name: &str,
    dependency_value: Value,
    workspace_dependencies: &Map<String, Value>,
) -> Result<Value> {
    let Some(table) = dependency_value.as_table() else {
        return Ok(dependency_value);
    };
    let inherits_workspace = table
        .get("workspace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !inherits_workspace {
        return Ok(dependency_value);
    }

    let base = workspace_dependencies
        .get(dependency_name)
        .cloned()
        .with_context(|| {
            format!("missing `[workspace.dependencies].{dependency_name}` in root manifest")
        })?;
    let mut resolved = match base {
        Value::String(version) => {
            let mut table = Map::new();
            table.insert("version".to_owned(), Value::String(version));
            table
        }
        Value::Table(table) => table,
        _ => bail!("unsupported workspace dependency format for `{dependency_name}`"),
    };

    for (key, value) in table {
        if key == "workspace" {
            continue;
        }
        if key == "features" {
            merge_feature_lists(&mut resolved, value.clone())?;
            continue;
        }
        resolved.insert(key.clone(), value.clone());
    }

    Ok(Value::Table(resolved))
}

fn merge_feature_lists(target: &mut Map<String, Value>, local_features: Value) -> Result<()> {
    let local_features = local_features
        .as_array()
        .context("dependency features must be an array")?;
    let existing = target
        .get("features")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut merged = Vec::new();
    for feature in existing.into_iter().chain(local_features.iter().cloned()) {
        if merged.contains(&feature) {
            continue;
        }
        merged.push(feature);
    }
    target.insert("features".to_owned(), Value::Array(merged));
    Ok(())
}

fn package_version(flattened_manifest: &str) -> Result<String> {
    let manifest = parse_toml(flattened_manifest)?;
    package_field_string(&manifest, "version")
}

fn package_field_string(manifest: &Map<String, Value>, field: &str) -> Result<String> {
    manifest
        .get("package")
        .and_then(Value::as_table)
        .and_then(|package| package.get(field))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .with_context(|| format!("flattened manifest is missing `package.{field}`"))
}

fn parse_toml(contents: &str) -> Result<Map<String, Value>> {
    let value: Value = toml::from_str(contents).context("invalid TOML")?;
    value
        .as_table()
        .cloned()
        .context("expected TOML document to be a table")
}

fn is_dependency_table_name(name: &str) -> bool {
    matches!(
        name,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    )
}

fn write_string(path: PathBuf, contents: &str) -> Result<()> {
    fs::write(&path, contents).with_context(|| format!("failed to write `{}`", path.display()))
}

fn copy_file(from: PathBuf, to: PathBuf) -> Result<()> {
    fs::copy(&from, &to)
        .with_context(|| format!("failed to copy `{}` to `{}`", from.display(), to.display()))?;
    Ok(())
}

fn copy_if_exists(from: PathBuf, to: PathBuf) -> Result<()> {
    if !from.is_file() {
        return Ok(());
    }
    copy_file(from, to)
}

fn copy_tree(from: PathBuf, to: PathBuf) -> Result<()> {
    for entry in WalkDir::new(&from).sort_by_file_name() {
        let entry = entry?;
        let source = entry.path();
        let relative = source
            .strip_prefix(&from)
            .with_context(|| format!("failed to compute path inside `{}`", from.display()))?;
        let destination = to.join(relative);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)
                .with_context(|| format!("failed to create `{}`", destination.display()))?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create `{}`", parent.display()))?;
        }
        fs::copy(source, &destination).with_context(|| {
            format!(
                "failed to copy `{}` to `{}`",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use flate2::read::GzDecoder;
    use tar::Archive;
    use tempfile::TempDir;
    use toml::Value;

    use super::package_checkleft_source_archive;

    #[test]
    fn packages_standalone_archive_with_flattened_manifest_and_bazel_files() {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir_all(repo.path().join("tools/checkleft/src")).expect("mkdir src");
        fs::create_dir_all(repo.path().join("tools/checkleft/api/javascript"))
            .expect("mkdir api/javascript");
        fs::create_dir_all(repo.path().join("tools/checkleft/bazel")).expect("mkdir bazel");
        fs::write(repo.path().join(".bazelversion"), "8.4.0\n").expect("write bazelversion");
        fs::write(
            repo.path().join("Cargo.toml"),
            r#"[workspace]
members = ["tools/checkleft"]
resolver = "2"

[workspace.package]
edition = "2024"

[workspace.dependencies]
anyhow = "1.0.97"
clap = "4.5.39"
flate2 = "1.1.5"
serde = "1.0.219"
tar = "0.4.44"
tokio = "1.45.0"
toml = "1.0.7"
walkdir = "2.5.0"
"#,
        )
        .expect("write workspace manifest");
        fs::write(repo.path().join("Cargo.lock"), "# mock lockfile\n").expect("write lockfile");
        fs::write(
            repo.path().join("tools/checkleft/Cargo.toml"),
            r#"[package]
name = "checkleft"
version = "0.1.2"
edition.workspace = true
workspace = "../.."
description = "demo"
license = "Apache-2.0"
readme = "README.md"
rust-version = "1.93.1"

[lib]
path = "src/lib.rs"

[[bin]]
name = "checkleft"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
clap = { workspace = true, features = ["derive"] }
flate2 = { workspace = true }
serde = { workspace = true, features = ["derive"] }
tar = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
toml = { workspace = true }
walkdir = { workspace = true }
"#,
        )
        .expect("write package manifest");
        fs::write(
            repo.path().join("tools/checkleft/src/lib.rs"),
            "pub fn library() {}\n",
        )
        .expect("write lib");
        fs::write(
            repo.path().join("tools/checkleft/src/main.rs"),
            "fn main() {}\n",
        )
        .expect("write main");
        fs::write(
            repo.path().join("tools/checkleft/README.md"),
            "# checkleft\n",
        )
        .expect("write readme");
        fs::write(repo.path().join("tools/checkleft/LICENSE"), "Apache-2.0\n")
            .expect("write license");
        fs::write(
            repo.path().join("tools/checkleft/api/BUILD.bazel"),
            r#"load("@aspect_rules_js//js:defs.bzl", "js_library")
load("@aspect_rules_js//npm:defs.bzl", "npm_package")

js_library(
    name = "checkleft_exec_js",
    srcs = ["javascript/checkleft_exec.mjs"],
    types = ["javascript/checkleft_exec.d.ts"],
)

npm_package(
    name = "checkleft_exec_pkg",
    srcs = [
        ":checkleft_exec_js",
        "package.json",
    ],
    package = "@checkleft/exec",
    version = "0.0.0",
)
"#,
        )
        .expect("write api BUILD");
        fs::write(
            repo.path().join("tools/checkleft/api/package.json"),
            r#"{
  "name": "@checkleft/exec",
  "version": "0.0.0",
  "private": true,
  "type": "module"
}
"#,
        )
        .expect("write api package.json");
        fs::write(
            repo.path()
                .join("tools/checkleft/api/javascript/checkleft_exec.mjs"),
            "export function readRequest() { return {}; }\n",
        )
        .expect("write exec helper");
        fs::write(
            repo.path()
                .join("tools/checkleft/api/javascript/checkleft_exec.d.ts"),
            "export interface ExecCheckRequest {}\n",
        )
        .expect("write exec helper types");
        fs::write(
            repo.path().join("tools/checkleft/bazel/defs.bzl"),
            r#"_checkleft_launcher = rule(
    attrs = {
        "_checkleft_bin": attr.label(
            default = "//tools/checkleft:checkleft",
            executable = True,
            cfg = "target",
        ),
    },
)
"#,
        )
        .expect("write bazel defs");

        let archive_path =
            package_checkleft_source_archive(repo.path(), None).expect("package archive");
        assert_eq!(
            archive_path,
            repo.path().join("dist/checkleft-0.1.2-source.tgz")
        );

        let archive_file = fs::File::open(&archive_path).expect("open archive");
        let decoder = GzDecoder::new(archive_file);
        let mut archive = Archive::new(decoder);

        let temp_extract = TempDir::new().expect("extract tempdir");
        archive.unpack(temp_extract.path()).expect("unpack archive");

        let packaged_root = temp_extract.path().join("checkleft-0.1.2-source");
        let manifest =
            fs::read_to_string(packaged_root.join("Cargo.toml")).expect("read packaged manifest");
        let manifest_value: Value = toml::from_str(&manifest).expect("parse packaged manifest");
        let package = manifest_value
            .get("package")
            .and_then(Value::as_table)
            .expect("package table");
        assert_eq!(package.get("edition").and_then(Value::as_str), Some("2024"));
        let packaged_workspace = manifest_value
            .get("workspace")
            .and_then(Value::as_table)
            .expect("workspace table");
        assert_eq!(
            packaged_workspace.get("resolver").and_then(Value::as_str),
            Some("2")
        );
        let dependencies = manifest_value
            .get("dependencies")
            .and_then(Value::as_table)
            .expect("dependencies table");
        assert_eq!(
            dependencies
                .get("clap")
                .and_then(Value::as_table)
                .and_then(|clap| clap.get("version"))
                .and_then(Value::as_str),
            Some("4.5.39")
        );
        assert_eq!(
            dependencies
                .get("clap")
                .and_then(Value::as_table)
                .and_then(|clap| clap.get("features"))
                .and_then(Value::as_array)
                .map(|features| {
                    features
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                }),
            Some(vec!["derive"])
        );
        assert_eq!(
            dependencies
                .get("tokio")
                .and_then(Value::as_table)
                .and_then(|tokio| tokio.get("version"))
                .and_then(Value::as_str),
            Some("1.45.0")
        );
        assert_eq!(
            dependencies
                .get("tokio")
                .and_then(Value::as_table)
                .and_then(|tokio| tokio.get("features"))
                .and_then(Value::as_array)
                .map(|features| {
                    features
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                }),
            Some(vec!["macros", "rt-multi-thread"])
        );
        assert!(!manifest.contains("workspace = true"));
        assert!(!manifest.contains("workspace = \"../..\""));

        let module_bazel =
            fs::read_to_string(packaged_root.join("MODULE.bazel")).expect("read MODULE.bazel");
        assert!(module_bazel.contains("name = \"checkleft\""));
        assert!(module_bazel.contains("version = \"0.1.2\""));
        assert!(module_bazel.contains("rust_toolchains"));
        assert!(module_bazel.contains("checkleft_crates"));
        assert!(module_bazel.contains("aspect_rules_js"));
        assert!(module_bazel.contains("BAZEL_CONSUMPTION.md"));

        let consumption_doc = fs::read_to_string(packaged_root.join("BAZEL_CONSUMPTION.md"))
            .expect("read BAZEL_CONSUMPTION.md");
        assert!(consumption_doc.contains("register_toolchains"));
        assert!(consumption_doc.contains("archive_override"));
        assert!(consumption_doc.contains("local_path_override"));
        assert!(consumption_doc.contains("@checkleft//bazel:defs.bzl"));
        assert!(consumption_doc.contains("@checkleft//api:checkleft_exec_pkg"));
        assert!(consumption_doc.contains("1.93.1"));

        let build_bazel =
            fs::read_to_string(packaged_root.join("BUILD.bazel")).expect("read BUILD.bazel");
        assert!(build_bazel.contains("package(default_visibility = [\"//visibility:private\"])"));
        assert!(build_bazel.contains("visibility = [\"//visibility:public\"]"));
        let defs_bzl =
            fs::read_to_string(packaged_root.join("bazel/defs.bzl")).expect("read defs.bzl");
        assert!(defs_bzl.contains("default = \"//:checkleft\""));
        assert!(!defs_bzl.contains("default = \"//tools/checkleft:checkleft\""));
        assert!(packaged_root.join("api/BUILD.bazel").is_file());
        assert!(packaged_root.join("api/package.json").is_file());
        assert!(
            packaged_root
                .join("api/javascript/checkleft_exec.mjs")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(packaged_root.join(".bazelversion")).expect("read .bazelversion"),
            "8.4.0\n"
        );
        assert!(packaged_root.join("src/lib.rs").is_file());
        assert!(packaged_root.join("src/main.rs").is_file());
    }
}
