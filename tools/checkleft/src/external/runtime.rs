use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::exclusion::{DeclaredExclusion, ExclusionStatus};
use crate::fix::{ComponentFixOutcome, CopyBackReport, WritableSandbox};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff, SourceTree};
use crate::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};
use crate::path::validate_relative_path;

use super::component_bindings::Check as WitCheck;
use super::component_bindings::checkleft::check::types as wit_types;
use super::declarative::{run_declarative_check, run_declarative_check_with_progress};
use super::sandbox::{AccessScope, HostCeiling, create_sandbox};
use super::{
    EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1, ExternalCheckComponentLimits, ExternalCheckComponentPackage,
    ExternalCheckPackage, ExternalCheckPackageImplementation,
};

mod cwasm_cache;
pub use cwasm_cache::{ComponentAotCache, cache_file_name, precompile_into_cache_dir};

/// Base wall-clock budget for component-v1 checks (5 seconds). Used as the
/// fixed component of the proportional timeout formula when no explicit
/// `timeout_ms` override is set in the check manifest.
pub(crate) const BASE_COMPONENT_TIMEOUT_MS: u64 = 5_000;
/// Per-file wall-clock budget increment (100 ms per changed file). Combined
/// with `BASE_COMPONENT_TIMEOUT_MS` to form a proportional default timeout:
/// `effective_ms = BASE + PER_FILE * n_files`, clamped to
/// `HOST_CEILING_TIMEOUT_MS`.
pub(crate) const PER_FILE_COMPONENT_TIMEOUT_MS: u64 = 100;
/// Default memory cap for component-v1 checks (256 MiB).
pub(crate) const DEFAULT_COMPONENT_MAX_MEMORY_MB: u64 = 256;
/// Maximum timeout a manifest may request (5 minutes). Requests above this
/// are silently clamped so out-of-tree manifests cannot hang the host for an
/// unbounded duration. Sized to accommodate whole-repo changesets.
pub(crate) const HOST_CEILING_TIMEOUT_MS: u64 = 300_000;
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
            limiter: MemoryLimiter {
                max_bytes: max_memory_bytes,
            },
        })
    }
}

/// Background thread that increments the engine's epoch counter every
/// millisecond. Stores that set an epoch deadline will be interrupted after
/// approximately `timeout_ms` ticks, giving wall-clock timeout semantics.
///
/// A dead ticker silently disables all timeouts, so liveness is tracked via an
/// atomic flag that the thread clears on exit (whether normal or panic). Callers
/// check `is_alive()` before scheduling a timed check execution.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    /// Set to `false` by the ticker thread when it exits (via RAII guard), so a
    /// panicked ticker is detected rather than silently disabling timeouts.
    alive: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Arc<Engine>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));

        let stop_flag = Arc::clone(&stop);
        let alive_flag = Arc::clone(&alive);

        let handle = std::thread::Builder::new()
            .name("checkleft-epoch-ticker".to_owned())
            .spawn(move || {
                // RAII guard: clear `alive` on any exit path, including panics,
                // so a dead ticker is detectable rather than silently disabling
                // all epoch-based timeouts.
                struct AliveGuard(Arc<AtomicBool>);
                impl Drop for AliveGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _guard = AliveGuard(alive_flag);

                while !stop_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK_INTERVAL);
                    engine.increment_epoch();
                }
            })
            .expect("failed to spawn epoch ticker thread");
        Self {
            stop,
            alive,
            handle: Some(handle),
        }
    }

    /// Returns `false` if the ticker thread has exited (normally or due to a
    /// panic). Epoch-based timeouts are only reliable while this returns `true`.
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
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
        config_dir: &Path,
        effective_severity: Option<Severity>,
    ) -> Result<CheckResult>;

    /// Like [`Self::execute`] but accepts a progress callback that is called
    /// with the cumulative count of files processed after each file (per-file
    /// mode) or each chunk (batch mode). The default ignores the callback and
    /// delegates to [`Self::execute`]; override in executors that run
    /// declarative checks to provide live per-file/per-batch progress.
    #[allow(clippy::too_many_arguments)]
    fn execute_with_progress(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
        config_dir: &Path,
        effective_severity: Option<Severity>,
        _on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        self.execute(package, changeset, source_tree, config, config_dir, effective_severity)
    }

    /// Count the files in `changeset` that this check will actually process after
    /// applicability filtering. Used to seed the progress reporter with the correct
    /// per-check eligible count before execution begins.
    ///
    /// The default returns the full changeset size, which is correct for component
    /// checks (they receive the entire changeset). Declarative checks override this
    /// to apply their `applies_to` glob filter.
    fn eligible_file_count(
        &self,
        _package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        _config: &toml::Value,
    ) -> usize {
        changeset.changed_files.len()
    }

    /// Return the declared exclusions for a component check. `config_dir` is the
    /// directory of the CHECKS file (repo-root-relative), used to resolve any
    /// config-file-relative paths in the returned `depends_on` lists.
    ///
    /// Returns an empty vec for implementations that do not support exclusion auditing.
    fn declared_exclusions_for_component(
        &self,
        _package: &ExternalCheckPackage,
        _check_name: &str,
        _config_json: &str,
        _config_dir: &Path,
    ) -> Result<Vec<DeclaredExclusion>> {
        Ok(vec![])
    }

    /// Re-evaluate a single exclusion and report whether it is still needed.
    /// `file_content` is the current content of the depended-on file (repo-root-relative
    /// path already resolved by the caller), or `None` when the file was deleted.
    ///
    /// Returns `Unknown` for implementations that do not support exclusion auditing.
    fn evaluate_exclusion_for_component(
        &self,
        _package: &ExternalCheckPackage,
        _check_name: &str,
        _config_json: &str,
        _exclusion: &DeclaredExclusion,
        _file_content: Option<&str>,
    ) -> Result<ExclusionStatus> {
        Ok(ExclusionStatus::Unknown)
    }
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
        _config_dir: &Path,
        _effective_severity: Option<Severity>,
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
    ticker: EpochTicker,
    /// AOT `.cwasm` cache for component-v1 artifacts.  `None` when the cache
    /// directory could not be created (disk full, read-only FS, etc.); in that
    /// case every component-v1 invocation falls back to JIT compilation.
    component_cache: Option<ComponentAotCache>,
    /// Per-run cache of loaded audit components, keyed by `artifact_sha256`.
    /// Avoids re-reading the WASM bytes and re-computing the SHA-256 digest
    /// for every `declared_exclusions_for_component` / `evaluate_exclusion_for_component`
    /// call when many exclusion entries share the same component.
    audit_component_cache: Mutex<HashMap<String, Component>>,
}

impl DefaultExternalCheckExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize check runtime root {}", root.display()))?;
        if !root.is_dir() {
            bail!("check runtime root is not a directory: {}", root.display());
        }

        let engine = Arc::new(build_wasmtime_engine()?);
        let ticker = EpochTicker::start(Arc::clone(&engine));

        // Prefer the platform user cache dir (or env override) so `.cwasm` files
        // are not written into the repository working tree.  Fall back to an
        // in-tree `.checkleft-cwasm/` directory only when no platform cache dir
        // is available (e.g. no home directory in a minimal container).
        let cache_dir = cwasm_cache::default_cache_dir().unwrap_or_else(|| root.join(".checkleft-cwasm"));
        let component_cache = ComponentAotCache::open(&cache_dir).map(Some).unwrap_or_else(|_| {
            tracing::warn!(
                "failed to open .cwasm cache at {}; component-v1 will use JIT compilation",
                cache_dir.display()
            );
            None
        });

        Ok(Self {
            root,
            engine,
            ticker,
            component_cache,
            audit_component_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Construct an executor with an explicit AOT cache directory.
    ///
    /// Primarily used in tests and benchmarks where the caller controls the
    /// cache location.
    pub fn new_with_cache(root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize check runtime root {}", root.display()))?;
        if !root.is_dir() {
            bail!("check runtime root is not a directory: {}", root.display());
        }

        let engine = Arc::new(build_wasmtime_engine()?);
        let ticker = EpochTicker::start(Arc::clone(&engine));
        let component_cache = Some(ComponentAotCache::open(cache_dir)?);
        Ok(Self {
            root,
            engine,
            ticker,
            component_cache,
            audit_component_cache: Mutex::new(HashMap::new()),
        })
    }

    fn execute_component_check(
        &self,
        package: &ExternalCheckPackage,
        component: &ExternalCheckComponentPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
        config_dir: &Path,
    ) -> Result<CheckResult> {
        if !self.ticker.is_alive() {
            anyhow::bail!(
                "epoch ticker thread has died; cannot enforce execution timeout for check `{}`",
                package.id
            );
        }
        let load_start = Instant::now();
        let component_bytes = if let Some(bytes) = component.artifact_bytes {
            bytes.to_vec()
        } else {
            let artifact_path = self.resolve_artifact_path(&component.artifact_path)?;
            fs::read(&artifact_path)
                .with_context(|| format!("failed to read wasm artifact {}", artifact_path.display()))?
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

        let wasm_component =
            self.load_or_compile_component(&package.id, &component_bytes, &component.artifact_sha256)?;
        tracing::debug!(
            check_id = %package.id,
            elapsed_ms = load_start.elapsed().as_millis(),
            "component loaded"
        );

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
                config_dir,
            },
        )
    }

    /// Apply the WASM/component check's `fix-check` entry point over `fixable`,
    /// routing any edits the guest returns through the [`crate::fix`] copy-back
    /// core.
    ///
    /// W1 model: the fixable set is staged into a writable sandbox preopened
    /// **read-only** for the guest (so the guest needs no write capability). The
    /// guest returns `file-edit`s; the host validates each edit targets a staged
    /// file, applies them to the staged copies, and atomically copies back only
    /// the files that actually changed. A fixer error — or an edit outside the
    /// fixable set — aborts the fix and leaves the real tree untouched.
    ///
    /// `dest_root` is the real working-tree root the changed files are written
    /// back to (normally the runtime root). Returns
    /// [`ComponentFixOutcome::NotFixable`] when the check declares no fix.
    ///
    /// Note (v1 constraint): the guest reads only the staged fixable set, not its
    /// full declared `access-scope`. This suits the typed-edit fixer that rewrites
    /// the files it flags; a fixer needing broader read context is out of scope.
    #[allow(clippy::too_many_arguments)]
    pub fn fix_component_check(
        &self,
        package: &ExternalCheckPackage,
        component: &ExternalCheckComponentPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
        config_dir: &Path,
        fixable: &[PathBuf],
        dest_root: &Path,
    ) -> Result<ComponentFixOutcome> {
        if !self.ticker.is_alive() {
            anyhow::bail!(
                "epoch ticker thread has died; cannot enforce execution timeout for fix of check `{}`",
                package.id
            );
        }

        // Reuse the audit loader: it reads + sha-verifies + AOT-loads the bytes
        // and memoizes the result per run, exactly the lifecycle a fix needs.
        let wasm_component = self.load_component_for_audit(package, component)?;

        run_component_fix(
            &self.engine,
            &self.root,
            FixRun {
                package,
                check_name: &component.check_name,
                component: &wasm_component,
                limits: component.limits.as_ref(),
                changeset,
                source_tree,
                config,
                config_dir,
                fixable,
                dest_root,
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

    /// Load and compile the component for a lightweight exclusion-audit call.
    /// Uses the AOT cache when available, same as normal execution.
    ///
    /// Results are memoized in `audit_component_cache` for the duration of the
    /// run so that the stale-exclusion audit hashes each distinct component at
    /// most once, regardless of how many exclusion entries reference it.
    fn load_component_for_audit(
        &self,
        package: &ExternalCheckPackage,
        component: &ExternalCheckComponentPackage,
    ) -> Result<Component> {
        // Fast path: component already loaded for this run.
        if let Some(cached) = self
            .audit_component_cache
            .lock()
            .unwrap()
            .get(&component.artifact_sha256)
        {
            return Ok(cached.clone());
        }

        let component_bytes = if let Some(bytes) = component.artifact_bytes {
            bytes.to_vec()
        } else {
            let artifact_path = self.resolve_artifact_path(&component.artifact_path)?;
            fs::read(&artifact_path)
                .with_context(|| format!("failed to read wasm artifact {}", artifact_path.display()))?
        };
        let actual_sha256 = sha256_hex(&component_bytes);
        if actual_sha256 != component.artifact_sha256 {
            bail!(
                "artifact sha256 mismatch for component package `{}`: expected `{}`, got `{}`",
                package.id,
                component.artifact_sha256,
                actual_sha256
            );
        }
        let wasm_component =
            self.load_or_compile_component(&package.id, &component_bytes, &component.artifact_sha256)?;
        self.audit_component_cache
            .lock()
            .unwrap()
            .insert(component.artifact_sha256.clone(), wasm_component.clone());
        Ok(wasm_component)
    }
}

impl ExternalCheckExecutor for DefaultExternalCheckExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
        config_dir: &Path,
        effective_severity: Option<Severity>,
    ) -> Result<CheckResult> {
        match &package.implementation {
            ExternalCheckPackageImplementation::Component(component) => {
                self.execute_component_check(package, component, changeset, source_tree, config, config_dir)
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
                run_declarative_check(
                    &self.root,
                    &package.id,
                    declarative,
                    changeset,
                    config,
                    effective_severity,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_with_progress(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
        config_dir: &Path,
        effective_severity: Option<Severity>,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        match &package.implementation {
            ExternalCheckPackageImplementation::Component(component) => {
                // Component checks are opaque wasm calls — no per-file granularity.
                self.execute_component_check(package, component, changeset, source_tree, config, config_dir)
            }
            ExternalCheckPackageImplementation::Declarative(declarative) => {
                if package.runtime != EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1 {
                    bail!(
                        "unsupported external runtime `{}` for declarative package `{}`",
                        package.runtime,
                        package.id
                    );
                }
                run_declarative_check_with_progress(
                    &self.root,
                    &package.id,
                    declarative,
                    changeset,
                    config,
                    effective_severity,
                    on_file_processed,
                )
            }
        }
    }

    fn eligible_file_count(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> usize {
        match &package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => {
                super::declarative::eligible_file_count(&self.root, d, changeset, config)
            }
            ExternalCheckPackageImplementation::Component(_) => changeset.changed_files.len(),
        }
    }

    fn declared_exclusions_for_component(
        &self,
        package: &ExternalCheckPackage,
        check_name: &str,
        config_json: &str,
        config_dir: &Path,
    ) -> Result<Vec<DeclaredExclusion>> {
        let ExternalCheckPackageImplementation::Component(component) = &package.implementation else {
            return Ok(vec![]);
        };
        if !self.ticker.is_alive() {
            anyhow::bail!(
                "epoch ticker thread has died; cannot enforce execution timeout for \
                 declared-exclusions in check `{check_name}` (`{}`)",
                package.id
            );
        }
        // Use the manifest timeout (or proportional default with n_files=0) so
        // the audit call is bounded by the same policy as the run-check phase.
        let (timeout_ticks, _) = resolve_component_limits(component.limits.as_ref(), 0);
        let wasm_component = self.load_component_for_audit(package, component)?;
        let raw = call_declared_exclusions(&self.engine, &wasm_component, check_name, config_json, timeout_ticks)
            .with_context(|| {
                format!(
                    "`declared-exclusions` failed for check `{check_name}` in `{}`",
                    package.id
                )
            })?;

        // Resolve config-file-relative paths to repo-root-relative PathBuf.
        let result = raw
            .into_iter()
            .map(|excl| DeclaredExclusion {
                entry: excl.entry,
                depends_on: excl.depends_on.into_iter().map(|rel| config_dir.join(rel)).collect(),
            })
            .collect();
        Ok(result)
    }

    fn evaluate_exclusion_for_component(
        &self,
        package: &ExternalCheckPackage,
        check_name: &str,
        config_json: &str,
        exclusion: &DeclaredExclusion,
        file_content: Option<&str>,
    ) -> Result<ExclusionStatus> {
        let ExternalCheckPackageImplementation::Component(component) = &package.implementation else {
            return Ok(ExclusionStatus::Unknown);
        };
        if !self.ticker.is_alive() {
            anyhow::bail!(
                "epoch ticker thread has died; cannot enforce execution timeout for \
                 evaluate-exclusion in check `{check_name}` (`{}`)",
                package.id
            );
        }
        // Use the manifest timeout (or proportional default with n_files=0) so
        // the audit call is bounded by the same policy as the run-check phase.
        let (timeout_ticks, _) = resolve_component_limits(component.limits.as_ref(), 0);
        let wasm_component = self.load_component_for_audit(package, component)?;
        let wit_excl = wit_types::DeclaredExclusion {
            entry: exclusion.entry.clone(),
            depends_on: exclusion
                .depends_on
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        };
        let status = call_evaluate_exclusion(
            &self.engine,
            &wasm_component,
            check_name,
            config_json,
            wit_excl,
            file_content,
            timeout_ticks,
        )
        .with_context(|| {
            format!(
                "`evaluate-exclusion` failed for check `{check_name}` in `{}`",
                package.id
            )
        })?;
        Ok(lift_exclusion_status(status))
    }
}

/// Resolve the effective timeout and memory cap for a component-v1 execution.
/// Manifest overrides are clamped to the host ceiling so out-of-tree manifests
/// cannot grant themselves unbounded resources.
///
/// When `limits.timeout_ms` is `Some(t)`, it is used as an absolute override
/// (clamped to `HOST_CEILING_TIMEOUT_MS`). When `None`, the proportional
/// default applies: `BASE_COMPONENT_TIMEOUT_MS + PER_FILE_COMPONENT_TIMEOUT_MS
/// × n_files`, also clamped to the ceiling.
///
/// Returns `(timeout_ms, max_memory_bytes)`.
fn resolve_component_limits(limits: Option<&ExternalCheckComponentLimits>, n_files: usize) -> (u64, usize) {
    let timeout_ms = if let Some(explicit) = limits.and_then(|l| l.timeout_ms) {
        explicit.min(HOST_CEILING_TIMEOUT_MS)
    } else {
        let proportional =
            BASE_COMPONENT_TIMEOUT_MS.saturating_add(PER_FILE_COMPONENT_TIMEOUT_MS.saturating_mul(n_files as u64));
        proportional.min(HOST_CEILING_TIMEOUT_MS)
    };

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

/// Format up to five file paths from `changeset` for inclusion in error messages.
fn format_file_list(changeset: &ChangeSet) -> String {
    let files = &changeset.changed_files;
    if files.is_empty() {
        return "<no files>".to_owned();
    }
    let cap = files.len().min(5);
    let head: Vec<String> = files[..cap].iter().map(|f| f.path.display().to_string()).collect();
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

/// Map the static WIT `access-scope` variants to the host `AccessScope`.
/// Returns `None` for the `declared-files` variant, which requires a dynamic
/// component call (handled by `resolve_access_scope`).
fn lift_access_scope(scope: Option<&wit_types::AccessScope>) -> Option<AccessScope> {
    match scope {
        None | Some(wit_types::AccessScope::ModifiedOnly) => Some(AccessScope::ModifiedOnly),
        Some(wit_types::AccessScope::WholeRepo) => Some(AccessScope::WholeRepo),
        Some(wit_types::AccessScope::Globs(patterns)) => Some(AccessScope::Globs(patterns.clone())),
        Some(wit_types::AccessScope::DeclaredFiles) => None,
    }
}

/// Resolve the access scope from the check descriptor and, if the check declares
/// `declared-files` scope, call `declare-required-files` to get the concrete file
/// list and return `AccessScope::ExplicitFiles`.
///
/// For all static scopes (`modified-only`, `whole-repo`, `globs`) this is
/// essentially free — it just maps the WIT variant. For `declared-files`, it
/// instantiates the component with a changeset-only sandbox, calls the export,
/// and returns an `ExplicitFiles` scope built from the returned paths.
fn resolve_access_scope(
    engine: &Engine,
    linker: &Linker<HostState>,
    ceiling: &HostCeiling,
    run: &ComponentRun<'_>,
    wit_scope: Option<&wit_types::AccessScope>,
    max_memory_bytes: usize,
    timeout_ticks: u64,
) -> Result<AccessScope> {
    // Handle the static scopes first — no component call needed.
    if let Some(static_scope) = lift_access_scope(wit_scope) {
        return Ok(static_scope);
    }

    // Only `declared-files` reaches here. Build a temporary sandbox containing
    // only the changeset files so the check can read them to discover its
    // required file set (e.g. ThenChange targets), then use the returned paths
    // to build an ExplicitFiles scope for the real run-check sandbox.
    let temp_sandbox = create_sandbox(run.changeset, AccessScope::ModifiedOnly, run.source_tree, ceiling)
        .with_context(|| {
            format!(
                "failed to build temporary sandbox for declare-required-files in check `{}`",
                run.package.id
            )
        })?;

    let host_state = HostState::with_sandbox_root(temp_sandbox.root.path(), max_memory_bytes).with_context(|| {
        format!(
            "failed to configure WASI context for declare-required-files in check `{}`",
            run.package.id
        )
    })?;
    let mut store = Store::new(engine, host_state);
    // Apply the same wall-clock budget as run-check: the declare call runs with
    // real filesystem preopens (changeset sandbox), so an untrusted component can
    // loop here just as easily as in run-check. Unlike the list-checks Phase-1
    // call (no preopens, purely static metadata, EPOCH_DEADLINE_NEVER is safe),
    // this call must be bounded.
    store.set_epoch_deadline(timeout_ticks);

    let instance = wasmtime(linker.instantiate(&mut store, run.component)).with_context(|| {
        format!(
            "failed to instantiate component for declare-required-files in `{}`",
            run.package.id
        )
    })?;
    let bindings = wasmtime(WitCheck::new(&mut store, &instance)).with_context(|| {
        format!(
            "failed to bind component for declare-required-files in `{}`",
            run.package.id
        )
    })?;

    let scoped_config = scope_exclude_globs_to_repo(run.config, run.config_dir);
    let config_json =
        serde_json::to_string(&scoped_config).context("failed to serialize config for declare-required-files")?;
    let wit_changeset = lower_changeset(run.changeset, run.source_tree);

    let declared =
        wasmtime(bindings.call_declare_required_files(&mut store, run.check_name, &wit_changeset, &config_json))
            .map_err(|err| {
                if is_interrupt_error(&err) {
                    anyhow::anyhow!(
                        "check `{}` in component `{}` exceeded its {} ms wall-clock limit \
                         during declare-required-files",
                        run.check_name,
                        run.package.id,
                        timeout_ticks,
                    )
                } else {
                    err.context(format!(
                        "declare-required-files failed for check `{}` in component `{}`",
                        run.check_name, run.package.id
                    ))
                }
            })?;

    // temp_sandbox kept alive through the call above; drop explicitly for clarity.
    drop(temp_sandbox);

    let explicit_paths: Vec<PathBuf> = declared.into_iter().map(PathBuf::from).collect();
    Ok(AccessScope::ExplicitFiles(explicit_paths))
}

fn compile_component(engine: &Engine, package_id: &str, component_bytes: &[u8]) -> Result<Component> {
    wasmtime(Component::new(engine, component_bytes))
        .with_context(|| format!("failed to compile component for `{package_id}`"))
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
    /// Repo-root-relative directory of the CHECKS file that configures this check.
    config_dir: &'a Path,
}

fn run_component_check(engine: &Engine, root: &Path, run: ComponentRun) -> Result<CheckResult> {
    let (timeout_ticks, max_memory_bytes) = resolve_component_limits(run.limits, run.changeset.changed_files.len());
    let linker = build_component_v1_linker(engine)?;

    // Phase 1: instantiate with an empty WASI context (no preopens) to call
    // list-checks() and discover the check's declared access-scope. The
    // descriptor is purely static metadata; no filesystem access is needed.
    let descriptors = {
        let mut store = Store::new(engine, HostState::with_empty_wasi());
        // list-checks must never be interrupted by epoch; it returns static
        // metadata so there is no meaningful wall-clock bound to enforce here.
        store.set_epoch_deadline(EPOCH_DEADLINE_NEVER);
        let instance = wasmtime(linker.instantiate(&mut store, run.component))
            .with_context(|| format!("failed to instantiate component for `{}`", run.package.id))?;
        let bindings = wasmtime(WitCheck::new(&mut store, &instance))
            .with_context(|| format!("failed to bind component exports for `{}`", run.package.id))?;
        wasmtime(bindings.call_list_checks(&mut store))
            .with_context(|| format!("`list-checks` failed for component `{}`", run.package.id))?
    };

    let descriptor = descriptors.iter().find(|d| d.name == run.check_name).ok_or_else(|| {
        let exported: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
        anyhow::anyhow!(
            "component `{}` does not export a check named `{}`; available: [{}]",
            run.package.id,
            run.check_name,
            exported.join(", ")
        )
    })?;

    // Build the capability sandbox from the declared scope. Files outside
    // the scope are not materialized; the guest cannot name them.
    let ceiling = HostCeiling::new(root);
    let access_scope = resolve_access_scope(
        engine,
        &linker,
        &ceiling,
        &run,
        descriptor.access_scope.as_ref(),
        max_memory_bytes,
        timeout_ticks,
    )
    .with_context(|| format!("failed to resolve access scope for check `{}`", run.package.id))?;

    // Destructure run for Phase 2. All fields are references so there are no
    // ownership issues from the field accesses above.
    let ComponentRun {
        package,
        check_name,
        component,
        limits: _,
        changeset,
        source_tree,
        config,
        config_dir,
    } = run;

    let sandbox = create_sandbox(changeset, access_scope, source_tree, &ceiling)
        .with_context(|| format!("failed to create FS sandbox for check `{}`", package.id))?;

    // Phase 2: re-instantiate with a WASI context that preopens the sandbox
    // root at "/". The guest reads via std::fs with no checkleft-specific
    // call; enforcement is structural (only sandboxed files exist).
    let host_state = HostState::with_sandbox_root(sandbox.root.path(), max_memory_bytes)
        .with_context(|| format!("failed to configure WASI context for check `{}`", package.id))?;
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limiter);
    store.set_epoch_deadline(timeout_ticks);
    let instance = wasmtime(linker.instantiate(&mut store, component))
        .with_context(|| format!("failed to instantiate component for `{}`", package.id))?;
    let bindings = wasmtime(WitCheck::new(&mut store, &instance))
        .with_context(|| format!("failed to bind component exports for `{}`", package.id))?;

    let input = lower_check_input(changeset, source_tree, config, config_dir)?;
    let file_list = format_file_list(changeset);
    let run_result = wasmtime(bindings.call_run_check(&mut store, check_name, &input)).map_err(|err| {
        if is_interrupt_error(&err) {
            anyhow::anyhow!(
                "check `{}` in component `{}` exceeded its {} ms wall-clock limit \
                     while processing: {}",
                check_name,
                package.id,
                timeout_ticks,
                file_list,
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
        wit_types::CheckError::Failed(msg) => {
            anyhow::anyhow!("check `{}` in component `{}` failed: {}", check_name, package.id, msg)
        }
    })?;

    let mut result = CheckResult {
        check_id: package.id.clone(),
        findings: findings.into_iter().map(lift_finding).collect(),
    };
    apply_struct_exclusions(&mut result, config, config_dir);
    Ok(result)
}

/// Inputs for one component `fix-check` invocation over a fixable set.
///
/// Mirrors [`ComponentRun`] but adds the fixable set `F` and the copy-back
/// destination. All fields are references so the struct is cheap to build.
struct FixRun<'a> {
    package: &'a ExternalCheckPackage,
    check_name: &'a str,
    component: &'a Component,
    limits: Option<&'a ExternalCheckComponentLimits>,
    changeset: &'a ChangeSet,
    source_tree: &'a dyn SourceTree,
    config: &'a toml::Value,
    /// Repo-root-relative directory of the CHECKS file that configures this check.
    config_dir: &'a Path,
    /// Repo-relative fixable set `F`: the files this check is permitted to fix.
    fixable: &'a [PathBuf],
    /// Real working-tree root that changed files are copied back to.
    dest_root: &'a Path,
}

/// Invoke a component's `fix-check` export over the fixable set and route the
/// returned edits through the safety copy-back core (W1: host-applied edits,
/// read-only guest sandbox).
fn run_component_fix(engine: &Engine, root: &Path, run: FixRun) -> Result<ComponentFixOutcome> {
    let (timeout_ticks, max_memory_bytes) = resolve_component_limits(run.limits, run.fixable.len());

    // Stage EXACTLY the fixable set into a writable sandbox (force-copied, never
    // hardlinked). This single sandbox is both the guest's read surface (preopened
    // read-only below — W1 needs no guest write capability) and the host's apply
    // target; the staged set is the airlock domain that copy-back enforces.
    let ceiling = HostCeiling::new(root);
    let sandbox = WritableSandbox::stage(run.fixable, run.source_tree, &ceiling)
        .with_context(|| format!("failed to stage writable fix sandbox for check `{}`", run.package.id))?;

    // Once paths absent from the tree are dropped, an empty staged set is a clean
    // no-op: there is nothing to read, fix, or copy back.
    if sandbox.staged_paths().is_empty() {
        return Ok(ComponentFixOutcome::Applied(CopyBackReport::default()));
    }

    let linker = build_component_v1_linker(engine)?;
    let host_state = HostState::with_sandbox_root(sandbox.root_path(), max_memory_bytes)
        .with_context(|| format!("failed to configure WASI context for fix of check `{}`", run.package.id))?;
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limiter);
    store.set_epoch_deadline(timeout_ticks);
    let instance = wasmtime(linker.instantiate(&mut store, run.component))
        .with_context(|| format!("failed to instantiate component for fix of `{}`", run.package.id))?;
    let bindings = wasmtime(WitCheck::new(&mut store, &instance))
        .with_context(|| format!("failed to bind component exports for fix of `{}`", run.package.id))?;

    let input = lower_check_input(run.changeset, run.source_tree, run.config, run.config_dir)?;
    let file_list = format_file_list(run.changeset);
    let fix_result = wasmtime(bindings.call_fix_check(&mut store, run.check_name, &input)).map_err(|err| {
        if is_interrupt_error(&err) {
            anyhow::anyhow!(
                "check `{}` in component `{}` exceeded its {} ms wall-clock limit while fixing: {}",
                run.check_name,
                run.package.id,
                timeout_ticks,
                file_list,
            )
        } else {
            err.context(format!(
                "`fix-check` call failed for check `{}` in component `{}` while fixing: {}",
                run.check_name, run.package.id, file_list,
            ))
        }
    })?;

    let edits = match fix_result {
        Ok(edits) => edits,
        // `not-fixable` is the ordinary outcome for a check with no declared fix:
        // a no-op, not an error. The sandbox is dropped untouched on return.
        Err(wit_types::FixError::NotFixable) => return Ok(ComponentFixOutcome::NotFixable),
        Err(wit_types::FixError::UnknownCheck(name)) => bail!(
            "component `{}` does not know check `{}` for fix (list-checks/run-check dispatch passed)",
            run.package.id,
            name,
        ),
        Err(wit_types::FixError::Failed(msg)) => bail!(
            "fix for check `{}` in component `{}` failed: {}",
            run.check_name,
            run.package.id,
            msg,
        ),
    };

    // Apply the guest's edits to the staged copies. An edit outside the fixable
    // set, or one that does not apply, aborts here — the sandbox is dropped and
    // the real tree is left untouched (no copy-back ran).
    apply_edits_to_sandbox(&sandbox, edits)
        .with_context(|| format!("failed to apply fix edits for check `{}`", run.package.id))?;

    let changed = sandbox
        .detect_changes()
        .with_context(|| format!("failed to detect fixed files for check `{}`", run.package.id))?;
    let report = sandbox.copy_back(&changed, run.dest_root);
    Ok(ComponentFixOutcome::Applied(report))
}

/// Apply guest-returned `file-edit`s to the staged sandbox copies, in order.
///
/// Each edit must target a path in the staged fixable set `F`; an edit elsewhere
/// is an airlock violation and aborts the whole fix. The edit's `old-text` must
/// occur in the current file content — a non-applying (e.g. stale) edit is a hard
/// error so the fix aborts rather than silently dropping the change. Multiple
/// edits to one file apply sequentially against the running content.
fn apply_edits_to_sandbox(sandbox: &WritableSandbox, edits: Vec<wit_types::FileEdit>) -> Result<()> {
    let staged: BTreeSet<PathBuf> = sandbox.staged_paths().into_iter().collect();

    for edit in edits {
        let rel = PathBuf::from(&edit.path);
        validate_relative_path(&rel).with_context(|| format!("fix edit targets invalid path `{}`", edit.path))?;
        if !staged.contains(&rel) {
            bail!(
                "airlock violation: fix edit targets `{}`, which is not in the fixable set",
                edit.path
            );
        }

        let target = sandbox.root_path().join(&rel);
        let content = fs::read_to_string(&target)
            .with_context(|| format!("failed to read staged file for fix edit: {}", target.display()))?;
        let updated =
            apply_file_edit(&content, &edit).with_context(|| format!("fix edit for `{}` does not apply", edit.path))?;
        fs::write(&target, updated)
            .with_context(|| format!("failed to write fix edit to staged file: {}", target.display()))?;
    }
    Ok(())
}

/// Apply a single `file-edit` to `content`, replacing the first occurrence of
/// `old-text` with `new-text`.
///
/// Errors when `old-text` is empty (insert-only edits are unsupported in v1) or
/// is not present in `content` (the edit is stale and does not apply).
fn apply_file_edit(content: &str, edit: &wit_types::FileEdit) -> Result<String> {
    if edit.old_text.is_empty() {
        bail!("fix edit has empty old-text; insert-only edits are not supported");
    }
    if !content.contains(&edit.old_text) {
        bail!("old-text not found in file (the edit is stale or does not apply)");
    }
    Ok(content.replacen(&edit.old_text, &edit.new_text, 1))
}

/// Suppress findings host-side for the framework-level `exclude_structs`
/// grandfathering convention.
///
/// `exclude_structs` entries are authored relative to the CHECKS file's
/// directory; findings are repo-root-relative. Reconciling the two coordinate
/// systems is a host concern — the guest emits a finding for every violating
/// struct (in repo-relative coordinates, with no knowledge of `config_dir`) and
/// the host drops the ones that a CHECKS author has grandfathered. Two entry
/// forms are honored, mirroring the native check:
///
/// * `relative/path.rs::Name` — qualified. Exempts struct `Name` only in the
///   file at `config_dir/relative/path.rs`. A same-named struct in any other
///   file is still flagged.
/// * `Name` — simple. Exempts struct `Name` in any file within the CHECKS
///   file's subtree (`config_dir`). A same-named struct outside the subtree is
///   still flagged.
///
/// No-op when the check declares no `exclude_structs` (the common case), so this
/// is safe to call for every component check.
fn apply_struct_exclusions(result: &mut CheckResult, config: &toml::Value, config_dir: &Path) {
    let entries = config.get("exclude_structs").and_then(|v| v.as_array());
    let Some(entries) = entries else {
        return;
    };

    // Qualified `repo_path::Name` exemptions and subtree-scoped simple names.
    let mut qualified: std::collections::HashSet<(PathBuf, String)> = std::collections::HashSet::new();
    let mut simple: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in entries.iter().filter_map(|v| v.as_str()) {
        match entry.split_once("::") {
            Some((path_part, name)) => {
                qualified.insert((config_dir.join(path_part), name.to_owned()));
            }
            None => {
                simple.insert(entry.to_owned());
            }
        }
    }
    if qualified.is_empty() && simple.is_empty() {
        return;
    }

    result.findings.retain(|finding| {
        let Some(location) = &finding.location else {
            return true;
        };
        let Some(struct_name) = struct_name_from_finding(&finding.message) else {
            return true;
        };
        // Qualified exemption: exact (path, name) match.
        if qualified.contains(&(location.path.clone(), struct_name.to_owned())) {
            return false;
        }
        // Simple exemption: name matches and the file is within the CHECKS subtree.
        if simple.contains(struct_name) && path_within_subtree(&location.path, config_dir) {
            return false;
        }
        true
    });
}

/// Extract the struct name a giant-structs finding names. The message form is
/// ``struct `Name` has more than …``; the struct name is the first
/// backtick-delimited token. Returns `None` when no such token is present (the
/// finding is then never suppressed — fail-safe).
fn struct_name_from_finding(message: &str) -> Option<&str> {
    let rest = message.split_once('`')?.1;
    rest.split_once('`').map(|(name, _)| name)
}

/// True when `path` lies within `subtree` (repo-root-relative). An empty subtree
/// is the repo root and contains every path.
fn path_within_subtree(path: &Path, subtree: &Path) -> bool {
    subtree.as_os_str().is_empty() || path.starts_with(subtree)
}

// --- Exclusion-audit WIT calls (no filesystem, uses with_empty_wasi) ---

fn call_declared_exclusions(
    engine: &Engine,
    component: &Component,
    check_name: &str,
    config_json: &str,
    timeout_ticks: u64,
) -> Result<Vec<wit_types::DeclaredExclusion>> {
    let linker = build_component_v1_linker(engine)?;
    let mut store = Store::new(engine, HostState::with_empty_wasi());
    // No filesystem preopens here, but a malicious or buggy component can still
    // busy-loop. Apply the same wall-clock bound as run-check so the audit path
    // cannot stall the host indefinitely.
    store.set_epoch_deadline(timeout_ticks);
    let instance = wasmtime(linker.instantiate(&mut store, component))
        .context("failed to instantiate component for declared-exclusions")?;
    let bindings =
        wasmtime(WitCheck::new(&mut store, &instance)).context("failed to bind component for declared-exclusions")?;
    wasmtime(bindings.call_declared_exclusions(&mut store, check_name, config_json)).map_err(|err| {
        if is_interrupt_error(&err) {
            anyhow::anyhow!(
                "check `{}` exceeded its {} ms wall-clock limit during declared-exclusions",
                check_name,
                timeout_ticks,
            )
        } else {
            err.context("call_declared_exclusions failed")
        }
    })
}

fn call_evaluate_exclusion(
    engine: &Engine,
    component: &Component,
    check_name: &str,
    config_json: &str,
    exclusion: wit_types::DeclaredExclusion,
    file_content: Option<&str>,
    timeout_ticks: u64,
) -> Result<wit_types::ExclusionStatus> {
    let linker = build_component_v1_linker(engine)?;
    let mut store = Store::new(engine, HostState::with_empty_wasi());
    // No filesystem preopens here, but a malicious or buggy component can still
    // busy-loop. Apply the same wall-clock bound as run-check so the audit path
    // cannot stall the host indefinitely.
    store.set_epoch_deadline(timeout_ticks);
    let instance = wasmtime(linker.instantiate(&mut store, component))
        .context("failed to instantiate component for evaluate-exclusion")?;
    let bindings =
        wasmtime(WitCheck::new(&mut store, &instance)).context("failed to bind component for evaluate-exclusion")?;
    wasmtime(bindings.call_evaluate_exclusion(&mut store, check_name, config_json, &exclusion, file_content)).map_err(
        |err| {
            if is_interrupt_error(&err) {
                anyhow::anyhow!(
                    "check `{}` exceeded its {} ms wall-clock limit during evaluate-exclusion",
                    check_name,
                    timeout_ticks,
                )
            } else {
                err.context("call_evaluate_exclusion failed")
            }
        },
    )
}

fn lift_exclusion_status(s: wit_types::ExclusionStatus) -> ExclusionStatus {
    match s {
        wit_types::ExclusionStatus::LoadBearing => ExclusionStatus::LoadBearing,
        wit_types::ExclusionStatus::Stale(reason) => ExclusionStatus::Stale { reason },
        wit_types::ExclusionStatus::Unknown => ExclusionStatus::Unknown,
    }
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

fn lower_changeset(changeset: &ChangeSet, source_tree: &dyn SourceTree) -> wit_types::ChangeSet {
    let base_files: Vec<wit_types::BaseFile> = changeset
        .changed_files
        .iter()
        .filter(|f| matches!(f.kind, ChangeKind::Deleted | ChangeKind::Modified))
        .filter_map(|f| {
            source_tree
                .read_file_versioned(&f.path, crate::input::TreeVersion::Base)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map(|content| wit_types::BaseFile {
                    path: f.path.to_string_lossy().into_owned(),
                    content,
                })
        })
        .collect();

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
        base_files,
    }
}

fn lower_check_input(
    changeset: &ChangeSet,
    source_tree: &dyn SourceTree,
    config: &toml::Value,
    config_dir: &Path,
) -> Result<wit_types::CheckInput> {
    // The guest operates purely in repo-root-relative coordinates and never sees
    // the CHECKS file's directory. `exclude_files`/`exclude_globs` patterns are
    // authored relative to that directory, so the host rewrites them to
    // repo-relative globs here — the guest then matches repo-relative changeset
    // paths against repo-relative globs with no notion of `config_dir`.
    let scoped_config = scope_exclude_globs_to_repo(config, config_dir);
    let config_json =
        serde_json::to_string(&scoped_config).context("failed to serialize config to JSON for component input")?;
    Ok(wit_types::CheckInput {
        changeset: lower_changeset(changeset, source_tree),
        config_json,
    })
}

/// Rewrite the framework-level `exclude_files`/`exclude_globs` glob patterns in
/// `config` from CHECKS-file-relative to repo-root-relative by prefixing
/// `config_dir`.
///
/// Exclude globs are authored relative to the CHECKS file that declares them,
/// but the guest only ever sees repo-relative changeset paths. Reconciling the
/// two coordinate systems is a host concern (the host located and parsed the
/// CHECKS file, so it holds `config_dir`); resolving here keeps the directory
/// out of the sandboxed guest entirely. A repo-root CHECKS file (`config_dir`
/// empty) needs no rewrite. Non-glob config is returned untouched.
fn scope_exclude_globs_to_repo(config: &toml::Value, config_dir: &Path) -> toml::Value {
    let mut config = config.clone();
    if config_dir.as_os_str().is_empty() {
        return config;
    }
    let prefix = config_dir.to_string_lossy();
    if let Some(table) = config.as_table_mut() {
        for key in ["exclude_files", "exclude_globs"] {
            let Some(toml::Value::Array(globs)) = table.get_mut(key) else {
                continue;
            };
            for glob in globs.iter_mut() {
                if let toml::Value::String(pattern) = glob {
                    *pattern = format!("{prefix}/{pattern}");
                }
            }
        }
    }
    config
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
