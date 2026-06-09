use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Instance, Memory, Module, ResourceLimiter, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff, SourceTree};
use crate::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

use super::component_bindings::checkleft::check::types as wit_types;
use super::component_bindings::Check as WitCheck;
use super::sandbox::{AccessScope, HostCeiling, create_sandbox};
use super::{
    EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1,
    EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckArtifactPackage, ExternalCheckComponentLimits,
    ExternalCheckComponentPackage, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalCommandCapabilities, run_declarative_check,
};

mod cwasm_cache;
pub use cwasm_cache::ComponentAotCache;

const CORE_ENTRYPOINT_EXPORT: &str = "checkleft_run";
const COMPONENT_ENTRYPOINT_EXPORT: &str = "run";
const MEMORY_EXPORT: &str = "memory";
const INPUT_OFFSET: usize = 0;
const WASM_PAGE_SIZE_BYTES: usize = 65_536;
const EXECUTION_FUEL_LIMIT: u64 = 10_000_000;

/// Default wall-clock timeout for component-v1 checks (5 seconds).
pub(crate) const DEFAULT_COMPONENT_TIMEOUT_MS: u64 = 5_000;
/// Default memory cap for component-v1 checks (256 MiB).
pub(crate) const DEFAULT_COMPONENT_MAX_MEMORY_MB: u64 = 256;
/// Maximum timeout a manifest may request (30 seconds). Requests above this
/// are silently clamped so out-of-tree manifests cannot hang the host.
pub(crate) const HOST_CEILING_TIMEOUT_MS: u64 = 30_000;
/// Maximum memory a manifest may request (512 MiB). Requests above this are
/// silently clamped so out-of-tree manifests cannot exhaust host memory.
pub(crate) const HOST_CEILING_MAX_MEMORY_MB: u64 = 512;

/// Per-millisecond epoch tick interval — the ticker thread wakes up this often
/// and calls `Engine::increment_epoch`, giving ~1 ms timeout resolution.
const EPOCH_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

/// Sentinel epoch delta for stores that must never be interrupted by epoch.
/// `wasmtime::Store::set_epoch_deadline(delta)` computes `current_epoch + delta`
/// without saturation, so using `u64::MAX` would overflow if the ticker has
/// already advanced the epoch. `u64::MAX / 2` is still ~292 million years at
/// 1ms/tick and will not overflow for any plausible `current_epoch`.
const EPOCH_DEADLINE_NEVER: u64 = u64::MAX / 2;

/// Memory `ResourceLimiter` installed on every component-v1 store. Rejects
/// memory growth requests that would push the linear memory beyond `max_bytes`.
struct MemoryLimiter {
    max_bytes: usize,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        Ok(desired <= self.max_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        Ok(true)
    }
}

/// Host data threaded through the wasmtime `Store` for component-v1 execution.
///
/// Holds the WASI context (filesystem preopens, stdio, env) and the resource
/// table required by `WasiView`, plus a `MemoryLimiter`. Phase-1 stores use an
/// empty context (no preopens) so that `list-checks` can be called to discover
/// the access scope; phase-2 stores use a context preopened at the capability
/// sandbox root. The `limiter` field is uncapped (`usize::MAX`) for WASI-path
/// stores and set from manifest limits for the artifact-v1 path.
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limiter: MemoryLimiter,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl HostState {
    fn new(max_bytes: usize) -> Self {
        Self {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter { max_bytes },
        }
    }

    /// Build host state with no filesystem preopens. Used for the `list-checks`
    /// discovery phase, which must not touch the filesystem.
    fn with_empty_wasi() -> Self {
        Self::new(usize::MAX)
    }

    /// Build host state with the sandbox root preopened read-only at `"/"`.
    /// Used for the `run-check` execution phase.
    fn with_sandbox_root(sandbox_root: &Path) -> Result<Self> {
        let mut builder = WasiCtxBuilder::new();
        builder.preopened_dir(sandbox_root, "/", DirPerms::READ, FilePerms::READ)?;
        Ok(Self {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter { max_bytes: usize::MAX },
        })
    }
}

/// Background thread that increments the engine's epoch counter every
/// millisecond. Stores that set an epoch deadline will be interrupted after
/// approximately `timeout_ms` ticks, giving wall-clock timeout semantics.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Arc<Engine>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("checkleft-epoch-ticker".to_owned())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK_INTERVAL);
                    engine.increment_epoch();
                }
            })
            .expect("failed to spawn epoch ticker thread");
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

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
    engine: Arc<Engine>,
    _ticker: EpochTicker,
    /// AOT `.cwasm` cache for component-v1 artifacts.  `None` when the cache
    /// directory could not be created (disk full, read-only FS, etc.); in that
    /// case every component-v1 invocation falls back to JIT compilation.
    component_cache: Option<ComponentAotCache>,
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

        let engine = Arc::new(build_wasmtime_engine()?);
        let ticker = EpochTicker::start(Arc::clone(&engine));

        let cache_dir = root.join(".checkleft-cwasm");
        let component_cache = ComponentAotCache::open(&cache_dir)
            .map(Some)
            .unwrap_or_else(|_| {
                tracing::warn!(
                    "failed to open .cwasm cache at {}; component-v1 will use JIT compilation",
                    cache_dir.display()
                );
                None
            });

        Ok(Self {
            root,
            engine,
            _ticker: ticker,
            component_cache,
        })
    }

    /// Construct an executor with an explicit AOT cache directory.
    ///
    /// Primarily used in tests and benchmarks where the caller controls the
    /// cache location.
    pub fn new_with_cache(root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Result<Self> {
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

        let engine = Arc::new(build_wasmtime_engine()?);
        let ticker = EpochTicker::start(Arc::clone(&engine));
        let component_cache = Some(ComponentAotCache::open(cache_dir)?);
        Ok(Self {
            root,
            engine,
            _ticker: ticker,
            component_cache,
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
        // The core (sandbox-v1) path uses fuel for throttling, not epoch. Set
        // the epoch deadline to u64::MAX so the always-enabled epoch mechanism
        // does not interrupt this store — wasmtime defaults to 0 (immediate trap).
        store.set_epoch_deadline(EPOCH_DEADLINE_NEVER);

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
        // Legacy path uses fuel; disable epoch the same way as execute_core_artifact.
        store.set_epoch_deadline(EPOCH_DEADLINE_NEVER);
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

    /// Execute a component-v1 (WIT) artifact. Applies epoch-based deadline and
    /// memory `ResourceLimiter` from `limits`, falling back to generous defaults
    /// clamped by the host ceiling. Pass `None` for `limits` to use defaults.
    fn execute_component_v1_artifact(
        &self,
        package: &ExternalCheckPackage,
        artifact: &ExternalCheckArtifactPackage,
        _limits: Option<&ExternalCheckComponentLimits>,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let artifact_path = self.resolve_artifact_path(&artifact.artifact_path)?;
        let component_bytes = fs::read(&artifact_path)
            .with_context(|| format!("failed to read wasm artifact {}", artifact_path.display()))?;
        validate_artifact_sha256(package, artifact, &component_bytes)?;

        let wasm_component = self.load_or_compile_component(
            &package.id,
            &component_bytes,
            &artifact.artifact_sha256,
        )?;
        run_component_check(
            &self.engine,
            &self.root,
            package,
            &package.id,
            &wasm_component,
            changeset,
            source_tree,
            config,
        )
    }

    fn execute_component_check(
        &self,
        package: &ExternalCheckPackage,
        component: &ExternalCheckComponentPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let component_bytes = if let Some(bytes) = component.artifact_bytes {
            bytes.to_vec()
        } else {
            let artifact_path = self.resolve_artifact_path(&component.artifact_path)?;
            fs::read(&artifact_path).with_context(|| {
                format!("failed to read wasm artifact {}", artifact_path.display())
            })?
        };

        let actual_sha256 = sha256_hex(&component_bytes);
        if actual_sha256 != component.artifact_sha256 {
            bail!(
                "artifact sha256 mismatch for component package `{}` (artifact_path `{}`): \
                 expected `{}`, got `{}`",
                package.id,
                component.artifact_path,
                component.artifact_sha256,
                actual_sha256
            );
        }

        let wasm_component = self.load_or_compile_component(
            &package.id,
            &component_bytes,
            &component.artifact_sha256,
        )?;
        run_component_check(
            &self.engine,
            &self.root,
            package,
            &component.check_name,
            &wasm_component,
            changeset,
            source_tree,
            config,
        )
    }

    /// Load a `Component` from the AOT cache or compile it from bytes.
    ///
    /// When the cache is available the first call for a given `artifact_sha256`
    /// precompiles and stores the result; subsequent calls deserialize from disk
    /// in low milliseconds.  When the cache is unavailable (not created or disk
    /// error), falls back to JIT compilation on every call.
    fn load_or_compile_component(
        &self,
        package_id: &str,
        component_bytes: &[u8],
        artifact_sha256: &str,
    ) -> Result<Component> {
        if let Some(cache) = &self.component_cache {
            cache.load_or_compile(&self.engine, package_id, component_bytes, artifact_sha256)
        } else {
            compile_component(&self.engine, package_id, component_bytes)
        }
    }

    fn resolve_artifact_path(&self, artifact_path: &str) -> Result<PathBuf> {
        let path = Path::new(artifact_path);
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
        source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        match &package.implementation {
            ExternalCheckPackageImplementation::Component(component) => {
                self.execute_component_check(package, component, changeset, source_tree, config)
            }
            ExternalCheckPackageImplementation::Declarative(declarative) => {
                if package.runtime != EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1 {
                    bail!(
                        "unsupported external runtime `{}` for declarative package `{}`",
                        package.runtime,
                        package.id
                    );
                }
                // Framework-owned invocation: resolve declared binaries and run
                // them at the repo root. Sandboxing is deferred by design.
                run_declarative_check(&self.root, &package.id, declarative, changeset, config)
            }
            ExternalCheckPackageImplementation::Artifact(artifact) => {
                match package.runtime.as_str() {
                    EXTERNAL_CHECK_COMPONENT_RUNTIME_V1 => self.execute_component_v1_artifact(
                        package, artifact, None, changeset, source_tree, config,
                    ),
                    EXTERNAL_CHECK_RUNTIME_V1 => {
                        let command_capabilities =
                            ExternalCommandCapabilities::from_manifest(&package.capabilities)
                                .with_context(|| {
                                    format!(
                                        "invalid command capability declaration for package `{}`",
                                        package.id
                                    )
                                })?;
                        self.execute_artifact(
                            package,
                            artifact,
                            &command_capabilities,
                            changeset,
                            config,
                        )
                    }
                    _ => bail!(
                        "unsupported external runtime `{}` for artifact package `{}`",
                        package.runtime,
                        package.id
                    ),
                }
            }
        }
    }
}

/// Resolve the effective timeout and memory cap for a component-v1 execution.
/// Manifest overrides are clamped to the host ceiling so out-of-tree manifests
/// cannot grant themselves unbounded resources.
///
/// Returns `(timeout_ms, max_memory_bytes)`.
fn resolve_component_limits(limits: Option<&ExternalCheckComponentLimits>) -> (u64, usize) {
    let timeout_ms = limits
        .and_then(|l| l.timeout_ms)
        .unwrap_or(DEFAULT_COMPONENT_TIMEOUT_MS)
        .min(HOST_CEILING_TIMEOUT_MS);

    let max_memory_mb = limits
        .and_then(|l| l.max_memory_mb)
        .unwrap_or(DEFAULT_COMPONENT_MAX_MEMORY_MB)
        .min(HOST_CEILING_MAX_MEMORY_MB);

    let max_memory_bytes = (max_memory_mb as usize).saturating_mul(1024 * 1024);
    (timeout_ms, max_memory_bytes)
}

/// Returns `true` if `err` was caused by a wasmtime epoch-interruption trap.
fn is_interrupt_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<wasmtime::Trap>()
            .map_or(false, |t| *t == wasmtime::Trap::Interrupt)
    })
}

fn build_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    config.consume_fuel(true);

    Ok(wasmtime(Engine::new(&config)).context("failed to initialize Wasmtime engine")?)
}

/// Build a component linker that includes all WASI preview-2 host interfaces.
/// Required for component-v1 checks: the guest links against WASI for file I/O.
fn build_component_v1_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::<HostState>::new(engine);
    wasmtime(wasmtime_wasi::p2::add_to_linker_sync(&mut linker))
        .context("failed to add WASI interfaces to component linker")?;
    Ok(linker)
}

/// Map the WIT `access-scope` variant from a check descriptor to the host
/// `AccessScope` used by the sandbox module. `None` (absent) means the check
/// did not declare a scope and defaults to `ModifiedOnly`.
fn lift_access_scope(scope: Option<&wit_types::AccessScope>) -> AccessScope {
    match scope {
        None | Some(wit_types::AccessScope::ModifiedOnly) => AccessScope::ModifiedOnly,
        Some(wit_types::AccessScope::WholeRepo) => AccessScope::WholeRepo,
        Some(wit_types::AccessScope::Globs(patterns)) => AccessScope::Globs(patterns.clone()),
    }
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

fn configure_store_fuel<T>(store: &mut Store<T>) -> Result<()> {
    wasmtime(store.set_fuel(EXECUTION_FUEL_LIMIT)).context("failed to configure runtime fuel limit")
}

/// Instantiate a component from raw bytes and execute the named check via the
/// `list-checks` / `run-check` WIT interface. Shared by both the file-based
/// (`Component` implementation variant) and the legacy artifact-wrapped
/// (`Artifact` with `runtime = "component-v1"`) execution paths.
fn run_component_check(
    engine: &Engine,
    root: &Path,
    package: &ExternalCheckPackage,
    check_name: &str,
    component: &Component,
    changeset: &ChangeSet,
    source_tree: &dyn SourceTree,
    config: &toml::Value,
) -> Result<CheckResult> {
    let linker = build_component_v1_linker(engine)?;

    // Phase 1: instantiate with an empty WASI context (no preopens) to call
    // list-checks() and discover the check's declared access-scope. The
    // descriptor is purely static metadata; no filesystem access is needed.
    let descriptors = {
        let mut store = Store::new(engine, HostState::with_empty_wasi());
        configure_store_fuel(&mut store)?;
        let instance = wasmtime(linker.instantiate(&mut store, &component))
            .with_context(|| format!("failed to instantiate component for `{}`", package.id))?;
        let bindings = wasmtime(WitCheck::new(&mut store, &instance))
            .with_context(|| format!("failed to bind component exports for `{}`", package.id))?;
        wasmtime(bindings.call_list_checks(&mut store))
            .with_context(|| format!("`list-checks` failed for component `{}`", package.id))?
    };

    let descriptor = descriptors
        .iter()
        .find(|d| d.name == check_name)
        .ok_or_else(|| {
            let exported: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
            anyhow::anyhow!(
                "component `{}` does not export a check named `{}`; available: [{}]",
                package.id,
                check_name,
                exported.join(", ")
            )
        })?;

    let access_scope = lift_access_scope(descriptor.access_scope.as_ref());

    // Build the capability sandbox from the declared scope. Files outside
    // the scope are not materialized; the guest cannot name them.
    let ceiling = HostCeiling::new(root);
    let sandbox = create_sandbox(changeset, access_scope, source_tree, &ceiling)
        .with_context(|| format!("failed to create FS sandbox for check `{}`", package.id))?;

    // Phase 2: re-instantiate with a WASI context that preopens the sandbox
    // root at "/". The guest reads via std::fs with no checkleft-specific
    // call; enforcement is structural (only sandboxed files exist).
    let host_state = HostState::with_sandbox_root(sandbox.root.path()).with_context(|| {
        format!("failed to configure WASI context for check `{}`", package.id)
    })?;
    let mut store = Store::new(engine, host_state);
    configure_store_fuel(&mut store)?;
    let instance = wasmtime(linker.instantiate(&mut store, &component))
        .with_context(|| format!("failed to instantiate component for `{}`", package.id))?;
    let bindings = wasmtime(WitCheck::new(&mut store, &instance))
        .with_context(|| format!("failed to bind component exports for `{}`", package.id))?;

    let input = lower_check_input(changeset, config)?;
    let run_result = wasmtime(bindings.call_run_check(&mut store, check_name, &input))
        .with_context(|| {
            format!(
                "`run-check` call failed for check `{}` in component `{}`",
                check_name, package.id
            )
        })?;

    let findings = run_result.map_err(|e| match e {
        wit_types::CheckError::UnknownCheck(name) => anyhow::anyhow!(
            "component `{}` does not know check `{}` (list-checks validation passed)",
            package.id,
            name
        ),
        wit_types::CheckError::Failed(msg) => anyhow::anyhow!(
            "check `{}` in component `{}` failed: {}",
            check_name,
            package.id,
            msg
        ),
    })?;

    // `sandbox` is kept alive until here so the preopened directory persists
    // for the entire run-check call above.
    drop(sandbox);

    Ok(CheckResult {
        check_id: package.id.clone(),
        findings: findings.into_iter().map(lift_finding).collect(),
    })
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

// --- Type lowering: host types → WIT types ---

fn lower_change_kind(kind: ChangeKind) -> wit_types::ChangeKind {
    match kind {
        ChangeKind::Added => wit_types::ChangeKind::Added,
        ChangeKind::Modified => wit_types::ChangeKind::Modified,
        ChangeKind::Deleted => wit_types::ChangeKind::Deleted,
        ChangeKind::Renamed => wit_types::ChangeKind::Renamed,
    }
}

fn lower_changed_file(f: &ChangedFile) -> wit_types::ChangedFile {
    wit_types::ChangedFile {
        path: f.path.to_string_lossy().into_owned(),
        kind: lower_change_kind(f.kind),
        old_path: f.old_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    }
}

fn lower_diff_hunk(h: &DiffHunk) -> wit_types::DiffHunk {
    wit_types::DiffHunk {
        old_start: h.old_start as u32,
        old_lines: h.old_lines as u32,
        new_start: h.new_start as u32,
        new_lines: h.new_lines as u32,
        added_lines: h.added_lines as u32,
        removed_lines: h.removed_lines as u32,
    }
}

fn lower_file_diff(path: &Path, diff: &FileDiff) -> wit_types::FileDiff {
    wit_types::FileDiff {
        path: path.to_string_lossy().into_owned(),
        hunks: diff.hunks.iter().map(lower_diff_hunk).collect(),
    }
}

fn lower_changeset(changeset: &ChangeSet) -> wit_types::ChangeSet {
    wit_types::ChangeSet {
        changed_files: changeset.changed_files.iter().map(lower_changed_file).collect(),
        file_diffs: changeset
            .file_diffs
            .iter()
            .map(|(path, diff)| lower_file_diff(path, diff))
            .collect(),
        commit_description: changeset.commit_description.clone(),
        pr_description: changeset.pr_description.clone(),
        change_id: changeset.change_id.clone(),
        repository: changeset.repository.clone(),
    }
}

fn lower_check_input(changeset: &ChangeSet, config: &toml::Value) -> Result<wit_types::CheckInput> {
    let config_json = serde_json::to_string(config)
        .context("failed to serialize config to JSON for component input")?;
    Ok(wit_types::CheckInput {
        changeset: lower_changeset(changeset),
        config_json,
    })
}

// --- Type lifting: WIT types → host types ---

fn lift_severity(s: wit_types::Severity) -> Severity {
    match s {
        wit_types::Severity::Error => Severity::Error,
        wit_types::Severity::Warning => Severity::Warning,
        wit_types::Severity::Info => Severity::Info,
    }
}

fn lift_location(loc: wit_types::Location) -> Location {
    Location {
        path: PathBuf::from(loc.path),
        line: loc.line,
        column: loc.column,
    }
}

fn lift_file_edit(edit: wit_types::FileEdit) -> FileEdit {
    FileEdit {
        path: PathBuf::from(edit.path),
        old_text: edit.old_text,
        new_text: edit.new_text,
    }
}

fn lift_suggested_fix(fix: wit_types::SuggestedFix) -> SuggestedFix {
    SuggestedFix {
        description: fix.description,
        edits: fix.edits.into_iter().map(lift_file_edit).collect(),
    }
}

fn lift_finding(f: wit_types::Finding) -> Finding {
    Finding {
        severity: lift_severity(f.severity),
        message: f.message,
        location: f.location.map(lift_location),
        remediations: f.remediations,
        suggested_fix: f.suggested_fix.map(lift_suggested_fix),
    }
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

#[cfg(test)]
mod tests;
