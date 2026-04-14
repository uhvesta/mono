use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Instance, Memory, Module, Store};

use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding};

use super::{
    EXTERNAL_CHECK_EXEC_RUNTIME_V1, EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckArtifactPackage,
    ExternalCheckExecPackage, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalCommandCapabilities, ExternalSourcePackageBuilder,
    JavaScriptComponentSourcePackageBuilder, exec_protocol,
};

const CORE_ENTRYPOINT_EXPORT: &str = "checkleft_run";
const COMPONENT_ENTRYPOINT_EXPORT: &str = "run";
const MEMORY_EXPORT: &str = "memory";
const INPUT_OFFSET: usize = 0;
const WASM_PAGE_SIZE_BYTES: usize = 65_536;
const EXECUTION_FUEL_LIMIT: u64 = 10_000_000;
const CHECKLEFT_REPO_ROOT_ENV: &str = "CHECKLEFT_REPO_ROOT";
const CHECKLEFT_CHECK_ID_ENV: &str = "CHECKLEFT_CHECK_ID";
const BAZEL_BINDIR_ENV: &str = "BAZEL_BINDIR";
const EXEC_STDOUT_CAPTURE_LIMIT_BYTES: usize = 1_048_576;
const EXEC_STDERR_CAPTURE_LIMIT_BYTES: usize = 262_144;

#[derive(Debug)]
enum CoreArtifactExecutionError {
    ArtifactMismatch(anyhow::Error),
    Execution(anyhow::Error),
}

impl CoreArtifactExecutionError {
    fn mismatch(err: anyhow::Error) -> Self {
        Self::ArtifactMismatch(err)
    }

    fn execution(err: anyhow::Error) -> Self {
        Self::Execution(err)
    }
}

pub trait ExternalCheckExecutor: Send + Sync {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult>;
}

#[derive(Debug, Default)]
pub struct NoopExternalCheckExecutor;

impl ExternalCheckExecutor for NoopExternalCheckExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        _changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        _config: &toml::Value,
    ) -> Result<CheckResult> {
        bail!(
            "external check package `{}` resolved successfully but runtime execution is not implemented yet",
            package.id
        )
    }
}

pub struct DefaultExternalCheckExecutor {
    root: PathBuf,
    engine: Engine,
    source_package_builder: Arc<dyn ExternalSourcePackageBuilder>,
}

impl DefaultExternalCheckExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize check runtime root {}",
                root.display()
            )
        })?;
        if !root.is_dir() {
            bail!("check runtime root is not a directory: {}", root.display());
        }

        let engine = build_wasmtime_engine()?;

        let source_package_builder =
            Arc::new(JavaScriptComponentSourcePackageBuilder::new(root.clone()));
        Ok(Self {
            root,
            engine,
            source_package_builder,
        })
    }

    #[cfg(test)]
    fn with_source_package_builder(
        root: impl Into<PathBuf>,
        source_package_builder: Arc<dyn ExternalSourcePackageBuilder>,
    ) -> Result<Self> {
        let root = root.into();
        let root = root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize check runtime root {}",
                root.display()
            )
        })?;
        if !root.is_dir() {
            bail!("check runtime root is not a directory: {}", root.display());
        }

        let engine = build_wasmtime_engine()?;

        Ok(Self {
            root,
            engine,
            source_package_builder,
        })
    }

    fn execute_artifact(
        &self,
        package: &ExternalCheckPackage,
        artifact: &ExternalCheckArtifactPackage,
        command_capabilities: &ExternalCommandCapabilities,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let artifact_path = self.resolve_artifact_path(&artifact.artifact_path)?;
        let module_bytes = fs::read(&artifact_path)
            .with_context(|| format!("failed to read wasm artifact {}", artifact_path.display()))?;
        validate_artifact_sha256(package, artifact, &module_bytes)?;

        match self.execute_core_artifact(
            package,
            &module_bytes,
            command_capabilities,
            changeset,
            config,
        ) {
            Ok(result) => Ok(result),
            Err(CoreArtifactExecutionError::ArtifactMismatch(core_error)) => self
                .execute_component_artifact(
                    package,
                    &module_bytes,
                    command_capabilities,
                    changeset,
                    config,
                )
                .with_context(|| {
                    format!(
                        "failed to execute package `{}` as component after core mismatch: {core_error:#}",
                        package.id
                    )
                }),
            Err(CoreArtifactExecutionError::Execution(error)) => Err(error),
        }
    }

    fn execute_core_artifact(
        &self,
        package: &ExternalCheckPackage,
        module_bytes: &[u8],
        command_capabilities: &ExternalCommandCapabilities,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> std::result::Result<CheckResult, CoreArtifactExecutionError> {
        let module = compile_core_module(&self.engine, package.id.as_str(), module_bytes)
            .map_err(CoreArtifactExecutionError::mismatch)?;
        let mut store = Store::new(&self.engine, ());
        configure_store_fuel(&mut store).map_err(CoreArtifactExecutionError::execution)?;

        let instance = instantiate_core_module(&mut store, &module, package.id.as_str())
            .map_err(CoreArtifactExecutionError::mismatch)?;

        let memory = instance
            .get_memory(&mut store, MEMORY_EXPORT)
            .context("wasm module must export `memory`")
            .map_err(CoreArtifactExecutionError::mismatch)?;
        let run = get_core_run_function(&instance, &mut store)
            .map_err(CoreArtifactExecutionError::mismatch)?;

        let input =
            ExternalCheckRuntimeInput::with_capabilities(changeset, config, command_capabilities);
        let input_bytes = serde_json::to_vec(&input)
            .context("failed to encode runtime input payload as JSON")
            .map_err(CoreArtifactExecutionError::execution)?;

        ensure_memory_capacity(&memory, &mut store, INPUT_OFFSET, input_bytes.len())
            .map_err(CoreArtifactExecutionError::execution)?;
        write_memory(&memory, &mut store, INPUT_OFFSET, &input_bytes)
            .map_err(CoreArtifactExecutionError::execution)?;

        let input_offset = i32::try_from(INPUT_OFFSET).context("input offset does not fit in i32");
        let input_offset = input_offset.map_err(CoreArtifactExecutionError::execution)?;
        let input_len =
            i32::try_from(input_bytes.len()).context("runtime input length exceeds i32");
        let input_len = input_len.map_err(CoreArtifactExecutionError::execution)?;
        let output_range_encoded = call_core_run(&run, &mut store, input_offset, input_len)
            .map_err(CoreArtifactExecutionError::execution)?;
        let (output_offset, output_len) = decode_output_range(output_range_encoded)
            .map_err(CoreArtifactExecutionError::execution)?;

        ensure_memory_capacity(&memory, &mut store, output_offset, output_len)
            .map_err(CoreArtifactExecutionError::execution)?;
        let mut output_bytes = vec![0_u8; output_len];
        read_memory(&memory, &mut store, output_offset, &mut output_bytes)
            .map_err(CoreArtifactExecutionError::execution)?;

        let output: ExternalCheckRuntimeOutput = serde_json::from_slice(&output_bytes)
            .context("runtime output was not valid JSON CheckResult payload")
            .map_err(CoreArtifactExecutionError::execution)?;

        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: output.findings,
        })
    }

    fn execute_component_artifact(
        &self,
        package: &ExternalCheckPackage,
        component_bytes: &[u8],
        command_capabilities: &ExternalCommandCapabilities,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let component = compile_component(&self.engine, package.id.as_str(), component_bytes)?;
        let linker = Linker::<()>::new(&self.engine);
        let mut store = Store::new(&self.engine, ());
        configure_store_fuel(&mut store)?;
        let instance = instantiate_component(&linker, &mut store, &component, package.id.as_str())?;
        let run = get_component_run_function(&instance, &mut store)?;

        let input =
            ExternalCheckRuntimeInput::with_capabilities(changeset, config, command_capabilities);
        let input_json =
            serde_json::to_string(&input).context("failed to encode component runtime input")?;
        let (output_json,) = call_component_run(&run, &mut store, input_json)?;
        let output: ExternalCheckRuntimeOutput =
            serde_json::from_str(&output_json).context("component output was not valid JSON")?;

        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: output.findings,
        })
    }

    fn resolve_artifact_path(&self, artifact_path: &str) -> Result<PathBuf> {
        let path = Path::new(artifact_path);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        Ok(self.root.join(path))
    }

    fn execute_exec(
        &self,
        package: &ExternalCheckPackage,
        exec: &ExternalCheckExecPackage,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let executable_path = self.resolve_exec_path(&exec.executable_path)?;
        let input = exec_protocol::ExecCheckRequest {
            changeset: changeset.clone(),
            config: config.clone(),
        };
        let input_bytes =
            serde_json::to_vec(&input).context("failed to encode exec runtime input as JSON")?;

        let mut command = Command::new(&executable_path);
        command
            .args(&exec.args)
            .current_dir(&self.root)
            .env(CHECKLEFT_REPO_ROOT_ENV, &self.root)
            .env(CHECKLEFT_CHECK_ID_ENV, &package.id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if uses_bazel_bin_launcher(&self.root, &executable_path) {
            command.env(BAZEL_BINDIR_ENV, ".");
        }

        let mut child = command
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn exec runtime for package `{}` at {}",
                    package.id,
                    executable_path.display()
                )
            })?;

        let mut stdin = child
            .stdin
            .take()
            .context("failed to open exec runtime stdin pipe")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to open exec runtime stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to open exec runtime stderr pipe")?;

        let stdout_handle = spawn_stream_reader(
            stdout,
            EXEC_STDOUT_CAPTURE_LIMIT_BYTES,
            "exec runtime stdout",
        );
        let stderr_handle = spawn_stream_reader(
            stderr,
            EXEC_STDERR_CAPTURE_LIMIT_BYTES,
            "exec runtime stderr",
        );

        stdin
            .write_all(&input_bytes)
            .context("failed to write exec runtime input to stdin")?;
        drop(stdin);

        let status = child.wait().with_context(|| {
            format!(
                "failed waiting for exec runtime of package `{}`",
                package.id
            )
        })?;
        let stdout_bytes = join_stream_reader(stdout_handle, "exec runtime stdout")?;
        let stderr_bytes = join_stream_reader(stderr_handle, "exec runtime stderr")?;

        ensure_successful_exit(package, &executable_path, status, &stderr_bytes)?;

        let output: exec_protocol::ExecCheckResponse = serde_json::from_slice(&stdout_bytes)
            .with_context(|| {
                format!(
                    "exec runtime output for package `{}` was not valid JSON",
                    package.id
                )
            })?;

        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: output.findings,
        })
    }
    fn resolve_exec_path(&self, executable_path: &str) -> Result<PathBuf> {
        let path = Path::new(executable_path);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        Ok(self.root.join(path))
    }
}

impl ExternalCheckExecutor for DefaultExternalCheckExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        match &package.implementation {
            ExternalCheckPackageImplementation::Exec(exec) => {
                if package.runtime != EXTERNAL_CHECK_EXEC_RUNTIME_V1 {
                    bail!(
                        "unsupported external runtime `{}` for exec package `{}`",
                        package.runtime,
                        package.id
                    );
                }
                self.execute_exec(package, exec, changeset, config)
            }
            ExternalCheckPackageImplementation::Artifact(artifact) => {
                if package.runtime != EXTERNAL_CHECK_RUNTIME_V1 {
                    bail!(
                        "unsupported external runtime `{}` for artifact package `{}`",
                        package.runtime,
                        package.id
                    );
                }
                let command_capabilities =
                    ExternalCommandCapabilities::from_manifest(&package.capabilities)
                        .with_context(|| {
                            format!(
                                "invalid command capability declaration for package `{}`",
                                package.id
                            )
                        })?;
                self.execute_artifact(package, artifact, &command_capabilities, changeset, config)
            }
            ExternalCheckPackageImplementation::Source(source) => {
                if package.runtime != EXTERNAL_CHECK_RUNTIME_V1 {
                    bail!(
                        "unsupported external runtime `{}` for source package `{}`",
                        package.runtime,
                        package.id
                    );
                }
                let command_capabilities =
                    ExternalCommandCapabilities::from_manifest(&package.capabilities)
                        .with_context(|| {
                            format!(
                                "invalid command capability declaration for package `{}`",
                                package.id
                            )
                        })?;
                let built_artifact = self
                    .source_package_builder
                    .build_source_package(package, source)?;
                self.execute_artifact(
                    package,
                    &built_artifact,
                    &command_capabilities,
                    changeset,
                    config,
                )
            }
        }
    }
}

fn build_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);

    Ok(wasmtime(Engine::new(&config)).context("failed to initialize Wasmtime engine")?)
}

fn compile_core_module(engine: &Engine, package_id: &str, module_bytes: &[u8]) -> Result<Module> {
    wasmtime(Module::new(engine, module_bytes))
        .with_context(|| format!("failed to compile core wasm module for `{package_id}`"))
}

fn instantiate_core_module(
    store: &mut Store<()>,
    module: &Module,
    package_id: &str,
) -> Result<Instance> {
    wasmtime(Instance::new(store, module, &[]))
        .with_context(|| format!("failed to instantiate wasm module for `{package_id}`"))
}

fn get_core_run_function(
    instance: &Instance,
    store: &mut Store<()>,
) -> Result<wasmtime::TypedFunc<(i32, i32), i64>> {
    wasmtime(instance.get_typed_func::<(i32, i32), i64>(store, CORE_ENTRYPOINT_EXPORT))
        .with_context(|| {
            format!(
                "core wasm module must export `{CORE_ENTRYPOINT_EXPORT}` with signature (i32, i32) -> i64"
            )
        })
}

fn call_core_run(
    run: &wasmtime::TypedFunc<(i32, i32), i64>,
    store: &mut Store<()>,
    input_offset: i32,
    input_len: i32,
) -> Result<i64> {
    wasmtime(run.call(store, (input_offset, input_len)))
        .context("external wasm check execution failed")
}

fn compile_component(
    engine: &Engine,
    package_id: &str,
    component_bytes: &[u8],
) -> Result<Component> {
    wasmtime(Component::new(engine, component_bytes))
        .with_context(|| format!("failed to compile component for `{package_id}`"))
}

fn instantiate_component(
    linker: &Linker<()>,
    store: &mut Store<()>,
    component: &Component,
    package_id: &str,
) -> Result<wasmtime::component::Instance> {
    wasmtime(linker.instantiate(store, component))
        .with_context(|| format!("failed to instantiate component for `{package_id}`"))
}

fn get_component_run_function(
    instance: &wasmtime::component::Instance,
    store: &mut Store<()>,
) -> Result<wasmtime::component::TypedFunc<(String,), (String,)>> {
    wasmtime(instance.get_typed_func::<(String,), (String,)>(store, COMPONENT_ENTRYPOINT_EXPORT))
        .with_context(|| {
            format!(
                "component must export `{COMPONENT_ENTRYPOINT_EXPORT}` with signature (string) -> (string)"
            )
        })
}

fn call_component_run(
    run: &wasmtime::component::TypedFunc<(String,), (String,)>,
    store: &mut Store<()>,
    input_json: String,
) -> Result<(String,)> {
    wasmtime(run.call(store, (input_json,))).context("external component check execution failed")
}

fn configure_store_fuel(store: &mut Store<()>) -> Result<()> {
    wasmtime(store.set_fuel(EXECUTION_FUEL_LIMIT)).context("failed to configure runtime fuel limit")
}

fn write_memory(memory: &Memory, store: &mut Store<()>, offset: usize, bytes: &[u8]) -> Result<()> {
    any_result(memory.write(store, offset, bytes))
        .context("failed to write runtime input into wasm memory")
}

fn read_memory(
    memory: &Memory,
    store: &mut Store<()>,
    offset: usize,
    bytes: &mut [u8],
) -> Result<()> {
    any_result(memory.read(store, offset, bytes))
        .context("failed to read runtime output from wasm memory")
}

#[derive(Serialize)]
struct ExternalCheckRuntimeInput<'a> {
    changeset: &'a ChangeSet,
    config: &'a toml::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<ExternalCheckRuntimeCapabilities>,
}

impl<'a> ExternalCheckRuntimeInput<'a> {
    fn with_capabilities(
        changeset: &'a ChangeSet,
        config: &'a toml::Value,
        command_capabilities: &ExternalCommandCapabilities,
    ) -> Self {
        Self {
            changeset,
            config,
            capabilities: Some(ExternalCheckRuntimeCapabilities {
                commands: command_capabilities.allowed_commands().to_vec(),
                command_timeout_ms: command_capabilities.timeout_ms(),
                max_stdout_bytes: command_capabilities.max_stdout_bytes(),
                max_stderr_bytes: command_capabilities.max_stderr_bytes(),
            }),
        }
    }
}

#[derive(Serialize)]
struct ExternalCheckRuntimeCapabilities {
    commands: Vec<String>,
    command_timeout_ms: u64,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
}

#[derive(Deserialize)]
struct ExternalCheckRuntimeOutput {
    findings: Vec<Finding>,
}

fn validate_artifact_sha256(
    package: &ExternalCheckPackage,
    artifact: &ExternalCheckArtifactPackage,
    bytes: &[u8],
) -> Result<()> {
    let actual_sha256 = sha256_hex(bytes);
    if actual_sha256 == artifact.artifact_sha256 {
        return Ok(());
    }

    bail!(
        "artifact sha256 mismatch for package `{}` (path `{}`): expected `{}`, got `{}`",
        package.id,
        artifact.artifact_path,
        artifact.artifact_sha256,
        actual_sha256
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn ensure_memory_capacity(
    memory: &Memory,
    store: &mut Store<()>,
    offset: usize,
    len: usize,
) -> Result<()> {
    let required_size = offset
        .checked_add(len)
        .context("requested wasm memory range overflows usize")?;
    let current_size = memory.data_size(&mut *store);
    if required_size <= current_size {
        return Ok(());
    }

    let needed_bytes = required_size - current_size;
    let additional_pages = needed_bytes.div_ceil(WASM_PAGE_SIZE_BYTES);
    wasmtime(memory.grow(
        &mut *store,
        u64::try_from(additional_pages).context("page count does not fit in u64")?,
    ))
    .context("failed to grow wasm memory")?;
    Ok(())
}

fn decode_output_range(encoded: i64) -> Result<(usize, usize)> {
    let encoded = u64::try_from(encoded).context("runtime returned negative output range")?;
    let offset = usize::try_from((encoded >> 32) as u32).context("output offset does not fit")?;
    let len = usize::try_from((encoded & 0xffff_ffff) as u32).context("output len does not fit")?;
    Ok((offset, len))
}

fn wasmtime<T>(result: std::result::Result<T, wasmtime::Error>) -> Result<T> {
    result.map_err(anyhow::Error::from)
}

fn any_result<T, E>(result: std::result::Result<T, E>) -> Result<T>
where
    E: Into<anyhow::Error>,
{
    result.map_err(Into::into)
}

fn ensure_successful_exit(
    package: &ExternalCheckPackage,
    executable_path: &Path,
    status: ExitStatus,
    stderr_bytes: &[u8],
) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(stderr_bytes).trim().to_owned();
    let stderr_suffix = if stderr.is_empty() {
        String::new()
    } else {
        format!("; stderr: {stderr}")
    };
    bail!(
        "exec runtime for package `{}` at {} exited with status {}{}",
        package.id,
        executable_path.display(),
        status,
        stderr_suffix
    );
}

fn spawn_stream_reader<R>(
    mut reader: R,
    max_bytes: usize,
    stream_name: &'static str,
) -> thread::JoinHandle<Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = reader
                .read(&mut chunk)
                .with_context(|| format!("failed to read {stream_name}"))?;
            if read == 0 {
                break;
            }
            if output.len() + read > max_bytes {
                bail!("{stream_name} exceeded {max_bytes} bytes");
            }
            output.extend_from_slice(&chunk[..read]);
        }
        Ok(output)
    })
}

fn join_stream_reader(
    handle: thread::JoinHandle<Result<Vec<u8>>>,
    stream_name: &str,
) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{stream_name} reader thread panicked"))?
}

fn uses_bazel_bin_launcher(root: &Path, executable_path: &Path) -> bool {
    executable_path.starts_with(root.join("bazel-bin"))
}

#[cfg(test)]
mod tests;
