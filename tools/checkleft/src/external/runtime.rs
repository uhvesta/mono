use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff, SourceTree};
use crate::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

use super::component_bindings::checkleft::check::types as wit_types;
use super::component_bindings::Check as WitCheck;
use super::sandbox::{AccessScope, HostCeiling, create_sandbox};
use super::{
    EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1, ExternalCheckComponentLimits,
    ExternalCheckComponentPackage, ExternalCheckPackage, ExternalCheckPackageImplementation,
    run_declarative_check,
};

mod cwasm_cache;
pub use cwasm_cache::ComponentAotCache;

/// Fuel limit for component-v1 checks.
///
/// Fuel counts wasm instructions. The epoch-based wall-clock timeout is the
/// primary safety net (see `DEFAULT_COMPONENT_TIMEOUT_MS`). This limit is set
/// high enough that it never becomes the binding constraint for any realistic
/// check workload — a check processing hundreds of large Rust files through
/// `syn` would exhaust 10M fuel on the first file, but needs the full
/// wall-clock budget. Epoch interruption catches runaway loops; fuel is kept
/// only as a guard against pathological instruction counts that somehow slip
/// past epoch checkpoints.
const EXECUTION_FUEL_LIMIT: u64 = u64::MAX / 2;

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
/// the access scope; phase-2 stores preopen the capability sandbox root and
/// enforce the manifest memory cap via `store.limiter()`.
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
    fn with_sandbox_root(sandbox_root: &Path, max_memory_bytes: usize) -> Result<Self> {
        let mut builder = WasiCtxBuilder::new();
        builder.preopened_dir(sandbox_root, "/", DirPerms::READ, FilePerms::READ)?;
        Ok(Self {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter { max_bytes: max_memory_bytes },
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
            ComponentRun {
                package,
                check_name: &component.check_name,
                component: &wasm_component,
                limits: component.limits.as_ref(),
                changeset,
                source_tree,
                config,
            },
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
            .is_some_and(|t| *t == wasmtime::Trap::Interrupt)
    })
}

/// Returns `true` if `err` was caused by the fuel budget being exhausted.
fn is_fuel_exhausted_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<wasmtime::Trap>()
            .is_some_and(|t| *t == wasmtime::Trap::OutOfFuel)
    })
}

/// Format up to five file paths from `changeset` for inclusion in error messages.
fn format_file_list(changeset: &ChangeSet) -> String {
    let files = &changeset.changed_files;
    if files.is_empty() {
        return "<no files>".to_owned();
    }
    let cap = files.len().min(5);
    let head: Vec<String> = files[..cap]
        .iter()
        .map(|f| f.path.display().to_string())
        .collect();
    if files.len() > 5 {
        format!("{} … ({} files total)", head.join(", "), files.len())
    } else {
        head.join(", ")
    }
}

fn build_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    config.consume_fuel(true);

    wasmtime(Engine::new(&config)).context("failed to initialize Wasmtime engine")
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

fn compile_component(
    engine: &Engine,
    package_id: &str,
    component_bytes: &[u8],
) -> Result<Component> {
    wasmtime(Component::new(engine, component_bytes))
        .with_context(|| format!("failed to compile component for `{package_id}`"))
}

fn configure_store_fuel<T>(store: &mut Store<T>) -> Result<()> {
    wasmtime(store.set_fuel(EXECUTION_FUEL_LIMIT)).context("failed to configure runtime fuel limit")
}

/// Instantiate a component from raw bytes and execute the named check via the
/// `list-checks` / `run-check` WIT interface.
struct ComponentRun<'a> {
    package: &'a ExternalCheckPackage,
    check_name: &'a str,
    component: &'a Component,
    limits: Option<&'a ExternalCheckComponentLimits>,
    changeset: &'a ChangeSet,
    source_tree: &'a dyn SourceTree,
    config: &'a toml::Value,
}

fn run_component_check(engine: &Engine, root: &Path, run: ComponentRun) -> Result<CheckResult> {
    let ComponentRun {
        package,
        check_name,
        component,
        limits,
        changeset,
        source_tree,
        config,
    } = run;
    let (timeout_ticks, max_memory_bytes) = resolve_component_limits(limits);
    let linker = build_component_v1_linker(engine)?;

    // Phase 1: instantiate with an empty WASI context (no preopens) to call
    // list-checks() and discover the check's declared access-scope. The
    // descriptor is purely static metadata; no filesystem access is needed.
    let descriptors = {
        let mut store = Store::new(engine, HostState::with_empty_wasi());
        // list-checks must never be interrupted by epoch; it returns static
        // metadata so there is no meaningful wall-clock bound to enforce here.
        store.set_epoch_deadline(EPOCH_DEADLINE_NEVER);
        configure_store_fuel(&mut store)?;
        let instance = wasmtime(linker.instantiate(&mut store, component))
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
    let host_state =
        HostState::with_sandbox_root(sandbox.root.path(), max_memory_bytes).with_context(|| {
            format!("failed to configure WASI context for check `{}`", package.id)
        })?;
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limiter);
    store.set_epoch_deadline(timeout_ticks);
    configure_store_fuel(&mut store)?;
    let instance = wasmtime(linker.instantiate(&mut store, component))
        .with_context(|| format!("failed to instantiate component for `{}`", package.id))?;
    let bindings = wasmtime(WitCheck::new(&mut store, &instance))
        .with_context(|| format!("failed to bind component exports for `{}`", package.id))?;

    let input = lower_check_input(changeset, config)?;
    let file_list = format_file_list(changeset);
    let run_result = wasmtime(bindings.call_run_check(&mut store, check_name, &input))
        .map_err(|err| {
            if is_interrupt_error(&err) {
                anyhow::anyhow!(
                    "check `{}` in component `{}` exceeded its {} ms wall-clock limit \
                     while processing: {}",
                    check_name, package.id, timeout_ticks, file_list,
                )
            } else if is_fuel_exhausted_error(&err) {
                anyhow::anyhow!(
                    "check `{}` in component `{}` exhausted its instruction budget \
                     while processing: {}",
                    check_name, package.id, file_list,
                )
            } else {
                err.context(format!(
                    "`run-check` call failed for check `{}` in component `{}` \
                     while processing: {}",
                    check_name, package.id, file_list,
                ))
            }
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn wasmtime<T>(result: std::result::Result<T, wasmtime::Error>) -> Result<T> {
    result.map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests;
