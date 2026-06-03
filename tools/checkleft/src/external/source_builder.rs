use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::path::validate_relative_path;

use super::{ExternalCheckArtifactPackage, ExternalCheckPackage, ExternalCheckSourcePackage};

const CHECKLEFT_CACHE_DIR_NAME: &str = "checkleft";
const CHECKLEFT_REPOS_CACHE_DIR: &str = "repos";
const CHECKLEFT_TOOLCHAINS_CACHE_DIR: &str = "toolchains";
const SOURCE_MODE_ARTIFACTS_DIR: &str = "source-mode/artifacts";
const JS_COMPONENTIZER_TOOLCHAIN_SOURCE_DIR: &str = "tools/checks_js_componentizer";
const JS_COMPONENTIZER_TOOLCHAINS_DIR: &str = "js-componentizer/toolchains";
const JS_COMPONENTIZER_PACKAGE_JSON: &str = "package.json";
const JS_COMPONENTIZER_LOCKFILE: &str = "pnpm-lock.yaml";
const JS_COMPONENTIZER_BUILD_SCRIPT: &str = "scripts/build_check.mjs";
const JS_COMPONENTIZER_SCRIPTS_DIR: &str = "scripts";
const JS_COMPONENTIZER_WIT_DIR: &str = "wit";
const JS_COMPONENTIZER_BOOTSTRAP_STAMP: &str = ".bootstrap.ok";
const SOURCE_BUILD_ABI_VERSION: &str = "source-build-v2";
const TOOLCHAIN_STATE_ABI_VERSION: &str = "js-componentizer-toolchain-v1";

pub trait ExternalSourcePackageBuilder: Send + Sync {
    fn build_source_package(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage>;
}

pub struct JavaScriptComponentSourcePackageBuilder {
    root: PathBuf,
    repo_cache_root: Option<PathBuf>,
    toolchain_cache_root: Option<PathBuf>,
    command_runner: Arc<dyn CommandRunner>,
}

impl JavaScriptComponentSourcePackageBuilder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            repo_cache_root: None,
            toolchain_cache_root: None,
            command_runner: Arc::new(ProcessCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_cache_roots(
        root: impl Into<PathBuf>,
        repo_cache_root: impl Into<PathBuf>,
        toolchain_cache_root: impl Into<PathBuf>,
        command_runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            root: root.into(),
            repo_cache_root: Some(repo_cache_root.into()),
            toolchain_cache_root: Some(toolchain_cache_root.into()),
            command_runner,
        }
    }

    fn build_javascript_component(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        let repo_cache_root = self.repo_cache_root()?;
        let toolchain_cache_root = self.toolchain_cache_root()?;
        let toolchain_source_dir = self.root.join(JS_COMPONENTIZER_TOOLCHAIN_SOURCE_DIR);
        let prepared_toolchain =
            self.prepare_toolchain(&toolchain_cache_root, &toolchain_source_dir)?;

        let source_inputs = self.collect_source_inputs(source)?;
        let cache_key = self.compute_cache_key(
            package,
            source,
            &prepared_toolchain.state_hash,
            &source_inputs,
        );
        let artifact_dir = repo_cache_root
            .join(SOURCE_MODE_ARTIFACTS_DIR)
            .join(cache_key);
        let artifact_path = artifact_dir.join("check.wasm");

        if !artifact_path.exists() {
            fs::create_dir_all(&artifact_dir).with_context(|| {
                format!(
                    "failed to create source-mode cache directory {}",
                    artifact_dir.display()
                )
            })?;

            let entry_path = self.resolve_relative_path(&source.entry).with_context(|| {
                format!(
                    "invalid source entry path `{}` for package `{}`",
                    source.entry, package.id
                )
            })?;
            let build_script = prepared_toolchain.dir.join(JS_COMPONENTIZER_BUILD_SCRIPT);

            self.command_runner.run(
                &prepared_toolchain.dir,
                "node",
                &[
                    build_script.to_string_lossy().into_owned(),
                    "--repo-root".to_owned(),
                    self.root.to_string_lossy().into_owned(),
                    "--entry".to_owned(),
                    entry_path.to_string_lossy().into_owned(),
                    "--out".to_owned(),
                    artifact_path.to_string_lossy().into_owned(),
                ],
            )?;
        }

        let artifact_bytes = fs::read(&artifact_path).with_context(|| {
            format!(
                "JS source adapter did not produce wasm artifact {}",
                artifact_path.display()
            )
        })?;
        let artifact_sha256 = sha256_hex(&artifact_bytes);

        Ok(ExternalCheckArtifactPackage {
            artifact_path: artifact_path.to_string_lossy().into_owned(),
            artifact_sha256,
            provenance: None,
        })
    }

    fn prepare_toolchain(
        &self,
        cache_root: &Path,
        toolchain_source_dir: &Path,
    ) -> Result<PreparedToolchain> {
        let toolchain_state_hash = self.compute_toolchain_state_hash(toolchain_source_dir)?;
        let toolchain_dir = cache_root
            .join(JS_COMPONENTIZER_TOOLCHAINS_DIR)
            .join(&toolchain_state_hash);

        self.sync_toolchain_inputs(toolchain_source_dir, &toolchain_dir)?;
        self.ensure_toolchain_bootstrapped(&toolchain_dir, &toolchain_state_hash)?;

        Ok(PreparedToolchain {
            dir: toolchain_dir,
            state_hash: toolchain_state_hash,
        })
    }

    fn ensure_toolchain_bootstrapped(
        &self,
        toolchain_dir: &Path,
        toolchain_state_hash: &str,
    ) -> Result<()> {
        fs::create_dir_all(toolchain_dir).with_context(|| {
            format!(
                "failed to create JS componentizer toolchain directory {}",
                toolchain_dir.display()
            )
        })?;

        let stamp = toolchain_dir.join(JS_COMPONENTIZER_BOOTSTRAP_STAMP);
        if stamp.exists() && toolchain_dir.join("node_modules").is_dir() {
            return Ok(());
        }

        self.command_runner
            .run(toolchain_dir, "node", &["--version".to_owned()])?;
        self.command_runner
            .run(toolchain_dir, "corepack", &["--version".to_owned()])?;
        self.command_runner.run(
            toolchain_dir,
            "corepack",
            &[
                "pnpm".to_owned(),
                "install".to_owned(),
                "--frozen-lockfile".to_owned(),
            ],
        )?;

        let stamp_parent = stamp.parent().context("bootstrap stamp has no parent")?;
        fs::create_dir_all(stamp_parent).with_context(|| {
            format!(
                "failed to create JS componentizer stamp directory {}",
                stamp_parent.display()
            )
        })?;
        fs::write(&stamp, toolchain_state_hash).with_context(|| {
            format!(
                "failed to write JS componentizer bootstrap stamp {}",
                stamp.display()
            )
        })?;
        Ok(())
    }

    fn repo_cache_root(&self) -> Result<PathBuf> {
        match &self.repo_cache_root {
            Some(cache_root) => Ok(cache_root.clone()),
            None => default_repo_cache_root(&self.root),
        }
    }

    fn toolchain_cache_root(&self) -> Result<PathBuf> {
        match &self.toolchain_cache_root {
            Some(cache_root) => Ok(cache_root.clone()),
            None => default_toolchain_cache_root(),
        }
    }

    fn compute_toolchain_state_hash(&self, toolchain_source_dir: &Path) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(TOOLCHAIN_STATE_ABI_VERSION);

        let inputs = self.collect_toolchain_inputs(toolchain_source_dir)?;
        for path in inputs {
            let relative = path.strip_prefix(toolchain_source_dir).with_context(|| {
                format!(
                    "toolchain input {} is not under {}",
                    path.display(),
                    toolchain_source_dir.display()
                )
            })?;
            hasher.update(relative_to_unix_string(relative).as_bytes());
            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read toolchain input {}", path.display()))?;
            hasher.update(bytes);
        }

        Ok(format!("{:x}", hasher.finalize()))
    }

    fn collect_toolchain_inputs(&self, toolchain_source_dir: &Path) -> Result<Vec<PathBuf>> {
        let package_json = toolchain_source_dir.join(JS_COMPONENTIZER_PACKAGE_JSON);
        let lockfile = toolchain_source_dir.join(JS_COMPONENTIZER_LOCKFILE);
        if !package_json.is_file() {
            bail!(
                "missing JS componentizer package manifest {}",
                package_json.display()
            );
        }
        if !lockfile.is_file() {
            bail!("missing JS componentizer lockfile {}", lockfile.display());
        }

        let mut inputs = vec![package_json, lockfile];
        let scripts_dir = toolchain_source_dir.join(JS_COMPONENTIZER_SCRIPTS_DIR);
        if scripts_dir.exists() {
            collect_files_recursively(&scripts_dir, &mut inputs)?;
        }
        let wit_dir = toolchain_source_dir.join(JS_COMPONENTIZER_WIT_DIR);
        if wit_dir.exists() {
            collect_files_recursively(&wit_dir, &mut inputs)?;
        }

        inputs.sort();
        Ok(inputs)
    }

    fn sync_toolchain_inputs(&self, source_dir: &Path, target_dir: &Path) -> Result<()> {
        copy_file(
            &source_dir.join(JS_COMPONENTIZER_PACKAGE_JSON),
            &target_dir.join(JS_COMPONENTIZER_PACKAGE_JSON),
        )?;
        copy_file(
            &source_dir.join(JS_COMPONENTIZER_LOCKFILE),
            &target_dir.join(JS_COMPONENTIZER_LOCKFILE),
        )?;
        copy_directory(
            &source_dir.join(JS_COMPONENTIZER_SCRIPTS_DIR),
            &target_dir.join(JS_COMPONENTIZER_SCRIPTS_DIR),
        )?;
        copy_directory(
            &source_dir.join(JS_COMPONENTIZER_WIT_DIR),
            &target_dir.join(JS_COMPONENTIZER_WIT_DIR),
        )?;
        Ok(())
    }

    fn collect_source_inputs(&self, source: &ExternalCheckSourcePackage) -> Result<Vec<PathBuf>> {
        let mut relative_paths = BTreeSet::new();
        relative_paths.insert(PathBuf::from(&source.entry));
        for source_path in &source.sources {
            relative_paths.insert(PathBuf::from(source_path));
        }

        let mut resolved_paths = Vec::with_capacity(relative_paths.len());
        for path in relative_paths {
            let absolute = self
                .resolve_relative_path(path.to_string_lossy().as_ref())
                .with_context(|| format!("invalid source path `{}`", path.display()))?;
            resolved_paths.push(absolute);
        }
        Ok(resolved_paths)
    }

    fn resolve_relative_path(&self, raw: &str) -> Result<PathBuf> {
        let path = Path::new(raw);
        validate_relative_path(path)?;
        let resolved = self.root.join(path);
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("source path does not exist: {}", resolved.display()))?;
        let root = self.root.canonicalize().with_context(|| {
            format!("failed to canonicalize source root {}", self.root.display())
        })?;
        if !canonical.starts_with(&root) {
            bail!(
                "source path escapes repository root: {}",
                resolved.display()
            );
        }
        Ok(canonical)
    }

    fn compute_cache_key(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
        lock_hash: &str,
        source_inputs: &[PathBuf],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(SOURCE_BUILD_ABI_VERSION);
        hasher.update(package.id.as_bytes());
        hasher.update(package.runtime.as_bytes());
        hasher.update(package.api_version.as_bytes());
        hasher.update(source.language.as_bytes());
        hasher.update(source.entry.as_bytes());
        hasher.update(source.build_adapter.as_bytes());
        hasher.update(lock_hash.as_bytes());

        for source_path in source_inputs {
            hasher.update(source_path.to_string_lossy().as_bytes());
            if let Ok(bytes) = fs::read(source_path) {
                hasher.update(bytes);
            }
        }

        format!("{:x}", hasher.finalize())
    }
}

struct PreparedToolchain {
    dir: PathBuf,
    state_hash: String,
}

impl ExternalSourcePackageBuilder for JavaScriptComponentSourcePackageBuilder {
    fn build_source_package(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        let language = source.language.trim();
        let build_adapter = source.build_adapter.trim();
        if !matches!(language, "javascript" | "typescript") {
            bail!(
                "unsupported source language `{language}` for package `{}`",
                package.id
            );
        }
        if build_adapter != "javascript-component" {
            bail!(
                "unsupported source build adapter `{build_adapter}` for package `{}`",
                package.id
            );
        }

        self.build_javascript_component(package, source)
    }
}

trait CommandRunner: Send + Sync {
    fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()>;
}

struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()> {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .with_context(|| format!("failed to run `{program}` in {}", cwd.display()))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let rendered_args = args.join(" ");
        bail!(
            "command `{program} {rendered_args}` failed in {} (status {}): stderr=`{stderr}` stdout=`{stdout}`",
            cwd.display(),
            output.status
        );
    }
}

fn default_repo_cache_root(root: &Path) -> Result<PathBuf> {
    let cache_home = default_cache_home()?;
    repo_cache_root_from_base(&cache_home, root)
}

fn default_toolchain_cache_root() -> Result<PathBuf> {
    let cache_home = default_cache_home()?;
    Ok(toolchain_cache_root_from_base(&cache_home))
}

fn repo_cache_root_from_base(cache_home: &Path, root: &Path) -> Result<PathBuf> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize repository root {}", root.display()))?;
    let repo_hash = sha256_hex(canonical_root.to_string_lossy().as_bytes());
    let repo_name = canonical_root
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.trim().is_empty())
        .map(sanitize_path_component)
        .unwrap_or_else(|| "repo".to_owned());

    Ok(cache_home
        .join(CHECKLEFT_CACHE_DIR_NAME)
        .join(CHECKLEFT_REPOS_CACHE_DIR)
        .join(format!("{repo_name}-{repo_hash}")))
}

fn toolchain_cache_root_from_base(cache_home: &Path) -> PathBuf {
    cache_home
        .join(CHECKLEFT_CACHE_DIR_NAME)
        .join(CHECKLEFT_TOOLCHAINS_CACHE_DIR)
}

fn default_cache_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME")
        && !path.is_empty() {
            return Ok(PathBuf::from(path));
        }

    let home = env::var_os("HOME").context("XDG_CACHE_HOME and HOME are unset")?;
    Ok(PathBuf::from(home).join(".cache"))
}

fn sanitize_path_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

fn relative_to_unix_string(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join("/")
}

fn collect_files_recursively(root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to read directory {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to determine file type for {}", path.display()))?;
        if file_type.is_dir() {
            collect_files_recursively(&path, output)?;
        } else if file_type.is_file() {
            output.push(path);
        }
    }

    Ok(())
}

fn copy_file(source: &Path, target: &Path) -> Result<()> {
    let target_parent = target
        .parent()
        .context("copied file target has no parent")?;
    fs::create_dir_all(target_parent).with_context(|| {
        format!(
            "failed to create copied file parent directory {}",
            target_parent.display()
        )
    })?;
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy toolchain input from {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn copy_directory(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read directory {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to determine file type for {}", path.display()))?;
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_directory(&path, &target_path)?;
        } else if file_type.is_file() {
            copy_file(&path, &target_path)?;
        }
    }

    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::external::{
        EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckCapabilities,
        ExternalCheckPackage, ExternalCheckPackageImplementation,
    };

    use super::{
        CommandRunner, ExternalSourcePackageBuilder, JavaScriptComponentSourcePackageBuilder,
    };

    #[derive(Default)]
    struct MockCommandRunner {
        calls: Mutex<Vec<(PathBuf, String, Vec<String>)>>,
    }

    impl CommandRunner for MockCommandRunner {
        fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()> {
            self.calls.lock().expect("lock calls").push((
                cwd.to_path_buf(),
                program.to_owned(),
                args.to_vec(),
            ));

            if program == "corepack"
                && args
                    .windows(2)
                    .any(|window| window == ["pnpm".to_owned(), "install".to_owned()])
            {
                std::fs::create_dir_all(cwd.join("node_modules")).expect("mkdir node_modules");
            }

            if program == "node" && args.first().is_some_and(|arg| arg.ends_with(".mjs")) {
                let out_index = args
                    .iter()
                    .position(|arg| arg == "--out")
                    .expect("out flag")
                    + 1;
                let output_path = Path::new(&args[out_index]);
                let wasm = wat::parse_str(
                    r#"(module
  (memory (export "memory") 1)
  (data (i32.const 16) "{\"findings\":[]}")
  (func (export "checkleft_run") (param i32 i32) (result i64)
    i64.const 68719476748
  )
)"#,
                )
                .expect("valid wat");
                std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir");
                std::fs::write(output_path, wasm).expect("write wasm");
            }

            assert!(cwd.exists(), "cwd must exist");
            Ok(())
        }
    }

    fn make_source_package(root: &Path) -> ExternalCheckPackage {
        std::fs::create_dir_all(root.join("checks/js")).expect("mkdir");
        std::fs::create_dir_all(root.join("tools/checks_js_componentizer/scripts"))
            .expect("mkdir scripts");
        std::fs::create_dir_all(root.join("tools/checks_js_componentizer/wit")).expect("mkdir wit");
        std::fs::write(
            root.join("tools/checks_js_componentizer/package.json"),
            r#"{"name":"checkleft-js-componentizer","private":true}"#,
        )
        .expect("package");
        std::fs::write(
            root.join("tools/checks_js_componentizer/pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .expect("lock");
        std::fs::write(
            root.join("tools/checks_js_componentizer/scripts/build_check.mjs"),
            "// test stub\n",
        )
        .expect("script");
        std::fs::write(
            root.join("tools/checks_js_componentizer/wit/check-runtime.wit"),
            "package checkleft:test;\n",
        )
        .expect("wit");
        std::fs::write(
            root.join("checks/js/check.js"),
            "export function run(input) { return input; }\n",
        )
        .expect("source");

        ExternalCheckPackage {
            id: "js-check".to_owned(),
            runtime: EXTERNAL_CHECK_RUNTIME_V1.to_owned(),
            api_version: EXTERNAL_CHECK_API_V1.to_owned(),
            capabilities: ExternalCheckCapabilities::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                crate::external::ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "checks/js/check.js".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: vec!["checks/js/check.js".to_owned()],
                },
            ),
        }
    }

    #[test]
    fn source_build_uses_cache_between_runs() {
        let temp = tempdir().expect("temp dir");
        let repo_cache_root = temp.path().join("repo-cache-root");
        let toolchain_cache_root = temp.path().join("toolchain-cache-root");
        let package = make_source_package(temp.path());
        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder = JavaScriptComponentSourcePackageBuilder::with_cache_roots(
            temp.path(),
            &repo_cache_root,
            &toolchain_cache_root,
            runner.clone(),
        );

        let first = builder
            .build_source_package(&package, source)
            .expect("first build");
        let second = builder
            .build_source_package(&package, source)
            .expect("second build");

        assert_eq!(first.artifact_path, second.artifact_path);
        assert_eq!(first.artifact_sha256, second.artifact_sha256);
        assert!(Path::new(&first.artifact_path).starts_with(&repo_cache_root));
        assert!(
            !temp.path().join(".checkleft-cache").exists(),
            "repo-local cache root should not be recreated"
        );
        assert!(
            !temp
                .path()
                .join("tools/checks_js_componentizer/node_modules")
                .exists(),
            "repo-local JS install should remain unused"
        );

        let calls = runner.calls.lock().expect("calls").clone();
        let compile_calls = calls
            .iter()
            .filter(|(_, program, args)| {
                program == "node" && args.first().is_some_and(|arg| arg.ends_with(".mjs"))
            })
            .count();
        assert_eq!(compile_calls, 1, "compile should be cached");
        let install_call_cwds = calls
            .iter()
            .filter(|(_, program, args)| {
                program == "corepack"
                    && args
                        .windows(2)
                        .any(|window| window == ["pnpm".to_owned(), "install".to_owned()])
            })
            .map(|(cwd, _, _)| cwd.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            install_call_cwds.len(),
            1,
            "toolchain install should be cached"
        );
        let install_cwd = install_call_cwds
            .into_iter()
            .next()
            .expect("install call should exist");
        assert!(
            install_cwd
                .starts_with(toolchain_cache_root.join(super::JS_COMPONENTIZER_TOOLCHAINS_DIR)),
            "toolchain install should happen in the shared toolchain cache"
        );
    }

    #[test]
    fn source_build_rebuilds_when_sources_change() {
        let temp = tempdir().expect("temp dir");
        let repo_cache_root = temp.path().join("repo-cache-root");
        let toolchain_cache_root = temp.path().join("toolchain-cache-root");
        let package = make_source_package(temp.path());
        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder = JavaScriptComponentSourcePackageBuilder::with_cache_roots(
            temp.path(),
            &repo_cache_root,
            &toolchain_cache_root,
            runner.clone(),
        );

        let first = builder
            .build_source_package(&package, source)
            .expect("first build");
        std::fs::write(
            temp.path().join("checks/js/check.js"),
            "export function run(input) { return input + 'x'; }\n",
        )
        .expect("rewrite source");
        let second = builder
            .build_source_package(&package, source)
            .expect("second build");

        assert_ne!(
            first.artifact_path, second.artifact_path,
            "cache key should include source bytes"
        );
    }

    #[test]
    fn repo_cache_root_is_scoped_by_canonical_root() {
        let temp = tempdir().expect("temp dir");
        let cache_home = temp.path().join("user-cache");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        std::fs::create_dir_all(&repo_a).expect("mkdir repo a");
        std::fs::create_dir_all(&repo_b).expect("mkdir repo b");

        let repo_a_cache =
            super::repo_cache_root_from_base(&cache_home, &repo_a).expect("repo a cache");
        let repo_b_cache =
            super::repo_cache_root_from_base(&cache_home, &repo_b).expect("repo b cache");

        assert_ne!(repo_a_cache, repo_b_cache);
        assert!(repo_a_cache.starts_with(&cache_home));
        assert!(repo_b_cache.starts_with(&cache_home));
    }

    #[test]
    fn toolchain_install_is_shared_across_repos() {
        let temp = tempdir().expect("temp dir");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        let repo_a_cache_root = temp.path().join("repo-a-cache");
        let repo_b_cache_root = temp.path().join("repo-b-cache");
        let toolchain_cache_root = temp.path().join("shared-toolchain-cache");
        let package_a = make_source_package(&repo_a);
        let package_b = make_source_package(&repo_b);
        let source_a = match &package_a.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let source_b = match &package_b.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder_a = JavaScriptComponentSourcePackageBuilder::with_cache_roots(
            &repo_a,
            &repo_a_cache_root,
            &toolchain_cache_root,
            runner.clone(),
        );
        let builder_b = JavaScriptComponentSourcePackageBuilder::with_cache_roots(
            &repo_b,
            &repo_b_cache_root,
            &toolchain_cache_root,
            runner.clone(),
        );

        let artifact_a = builder_a
            .build_source_package(&package_a, source_a)
            .expect("build repo a");
        let artifact_b = builder_b
            .build_source_package(&package_b, source_b)
            .expect("build repo b");

        assert!(
            Path::new(&artifact_a.artifact_path).starts_with(&repo_a_cache_root),
            "repo a artifact should stay repo-scoped"
        );
        assert!(
            Path::new(&artifact_b.artifact_path).starts_with(&repo_b_cache_root),
            "repo b artifact should stay repo-scoped"
        );

        let calls = runner.calls.lock().expect("calls").clone();
        let install_call_cwds = calls
            .iter()
            .filter(|(_, program, args)| {
                program == "corepack"
                    && args
                        .windows(2)
                        .any(|window| window == ["pnpm".to_owned(), "install".to_owned()])
            })
            .map(|(cwd, _, _)| cwd.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            install_call_cwds.len(),
            1,
            "matching repos should share one toolchain install"
        );
        assert!(
            install_call_cwds[0]
                .starts_with(toolchain_cache_root.join(super::JS_COMPONENTIZER_TOOLCHAINS_DIR)),
            "shared install should live under the shared toolchain cache"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_build_rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let outside = tempdir().expect("outside temp dir");
        std::fs::write(
            outside.path().join("check.js"),
            "export function run(input){return input;}",
        )
        .expect("write outside source");

        let package = make_source_package(temp.path());
        std::fs::remove_file(temp.path().join("checks/js/check.js")).expect("remove source file");
        symlink(
            outside.path().join("check.js"),
            temp.path().join("checks/js/check.js"),
        )
        .expect("create symlink");

        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder = JavaScriptComponentSourcePackageBuilder::with_cache_roots(
            temp.path(),
            temp.path().join("repo-cache-root"),
            temp.path().join("toolchain-cache-root"),
            runner,
        );

        let error = builder
            .build_source_package(&package, source)
            .expect_err("must reject escaping source path");
        let message = error.to_string();
        assert!(
            message.contains("escapes repository root") || message.contains("invalid source path")
        );
    }
}
