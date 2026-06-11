//! On-disk `.cwasm` cache for AOT-precompiled WebAssembly components.
//!
//! Cache entries are keyed by a SHA-256 digest of:
//!   `artifact_sha256 | wasmtime_version | engine_config_key | target_triple`
//!
//! This ensures cache misses on any axis that would produce an incompatible
//! precompiled artifact: a different source component, a different wasmtime
//! release, a different engine configuration, or a cross-compiled target.
//!
//! Writes are atomic (temp file + rename) so a partial write from a crash
//! never leaves a corrupt entry in the cache directory.
//!
//! ## Cache location
//!
//! By default the cache lives under the platform-standard user cache directory:
//! - macOS: `~/Library/Caches/checkleft/cwasm`
//! - Linux: `$XDG_CACHE_HOME/checkleft/cwasm` (fallback `~/.cache/checkleft/cwasm`)
//! - Windows: `%LOCALAPPDATA%\checkleft\cache\cwasm`
//!
//! Set `CHECKLEFT_CWASM_CACHE_DIR` to override the directory, e.g. in CI to
//! point at a shared disk-cache volume:
//!
//! ```text
//! CHECKLEFT_CWASM_CACHE_DIR=/mnt/ci-cache/cwasm checkleft ...
//! ```
//!
//! ## Bounded growth
//!
//! On each `open()`, `.cwasm` files whose mtime is older than
//! `CACHE_MAX_AGE_DAYS` are removed. Because the cache key already embeds the
//! wasmtime version and engine config, upgrading checkleft naturally orphans old
//! entries (different keys); the age sweep clears them without manual
//! intervention. The sweep is best-effort: errors are silently ignored so a
//! read-only filesystem or a permissions issue never prevents checkleft from
//! running.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use wasmtime::Engine;
use wasmtime::component::Component;

/// Wasmtime version injected by build.rs from the workspace Cargo.lock.
/// Must change whenever wasmtime is bumped because `.cwasm` files are not
/// portable across releases.
const WASMTIME_VERSION: &str = env!("CHECKLEFT_WASMTIME_VERSION");

/// Canonical string encoding the engine features we enable.
/// Update this constant whenever `build_wasmtime_engine` adds or removes a
/// feature flag — a stale string here would allow the cache to serve artifacts
/// compiled under a different configuration.
const ENGINE_CONFIG_KEY: &str = "component-model=true,fuel=false,cranelift=true";

/// Environment variable that overrides the AOT cache directory.
pub const CACHE_DIR_ENV_VAR: &str = "CHECKLEFT_CWASM_CACHE_DIR";

/// Maximum age of a cache entry before it is evicted on the next `open()`.
const CACHE_MAX_AGE_DAYS: u64 = 90;

/// Resolve the default AOT cache directory.
///
/// Resolution order:
/// 1. `$CHECKLEFT_CWASM_CACHE_DIR` — explicit override (useful in CI where
///    disk caches live at a custom mount point).
/// 2. Platform cache directory:
///    - macOS: `~/Library/Caches/checkleft/cwasm`
///    - Linux: `$XDG_CACHE_HOME/checkleft/cwasm` or `~/.cache/checkleft/cwasm`
///    - Windows: `%LOCALAPPDATA%\checkleft\cache\cwasm`
///
/// Returns `None` if neither a valid env override is set nor a platform cache
/// dir can be resolved (e.g. no home directory is available).  The caller
/// should then fall back to an in-tree path such as `{repo_root}/.checkleft-cwasm`.
pub fn default_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(CACHE_DIR_ENV_VAR) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    directories::ProjectDirs::from("", "", "checkleft").map(|proj| proj.cache_dir().join("cwasm"))
}

/// On-disk `.cwasm` cache directory.
///
/// One directory holds entries for all components that have passed through this
/// executor instance.  Each entry is a single file named
/// `{cache_key_sha256}.cwasm`.  The cache is safe for concurrent writers: each
/// writer races to rename a temp file into place; the loser's file is
/// equivalent.
#[derive(Debug)]
pub struct ComponentAotCache {
    dir: PathBuf,
}

impl ComponentAotCache {
    /// Open (or create) a cache rooted at `dir`.
    ///
    /// On each open, `.cwasm` files older than `CACHE_MAX_AGE_DAYS` days are
    /// evicted.  The eviction is best-effort: any errors are silently ignored
    /// so a slow/failing filesystem never prevents checkleft from running.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create .cwasm cache directory {}", dir.display()))?;
        let cache = Self { dir };
        let _ = cache.evict_old_entries();
        Ok(cache)
    }

    /// Return a compiled `Component`, loading from the on-disk cache if
    /// available or precompiling and caching otherwise.
    ///
    /// # Safety invariant
    ///
    /// `Component::deserialize_file` is `unsafe` because it trusts the file
    /// to have been produced by this engine's `precompile_component`.  The
    /// invariant is upheld by the cache key: every file in the cache directory
    /// was written by `Engine::precompile_component` with an engine whose
    /// `(wasmtime_version, engine_config)` matches the key prefix.  A file
    /// written by a different engine version will live under a different key
    /// and will never be deserialized by this instance.
    pub fn load_or_compile(
        &self,
        engine: &Engine,
        package_id: &str,
        component_bytes: &[u8],
        artifact_sha256: &str,
    ) -> Result<Component> {
        let cache_path = self.cache_path(artifact_sha256);

        if cache_path.exists() {
            match self.try_load_cached(engine, &cache_path) {
                Ok(component) => return Ok(component),
                Err(_) => {
                    // Corrupt or stale entry from a previous crash / partial write.
                    // Remove it and fall through to a fresh precompile.
                    let _ = fs::remove_file(&cache_path);
                }
            }
        }

        self.compile_and_cache(engine, package_id, component_bytes, &cache_path)
    }

    /// Load and deserialize a `.cwasm` file produced by this executor.
    fn try_load_cached(&self, engine: &Engine, path: &Path) -> Result<Component> {
        // SAFETY: every file stored at a cache path was written by
        // `Engine::precompile_component` using the same engine configuration
        // that `engine` encodes in the cache key.  The cache key includes the
        // wasmtime version and engine config hash, so a file from a different
        // engine variant cannot appear at this path.
        from_wasmtime(unsafe { Component::deserialize_file(engine, path) })
            .with_context(|| format!("failed to deserialize cached .cwasm {}", path.display()))
    }

    /// Precompile `component_bytes` and write the result atomically to `cache_path`.
    fn compile_and_cache(
        &self,
        engine: &Engine,
        package_id: &str,
        component_bytes: &[u8],
        cache_path: &Path,
    ) -> Result<Component> {
        let cwasm = from_wasmtime(engine.precompile_component(component_bytes))
            .with_context(|| format!("failed to precompile component for `{package_id}`"))?;

        write_atomically(cache_path, &cwasm).with_context(|| {
            format!(
                "failed to write .cwasm cache entry for `{package_id}` to {}",
                cache_path.display()
            )
        })?;

        // Deserialize from the bytes we already have in memory rather than
        // re-reading from disk — avoids a redundant I/O round-trip.
        //
        // SAFETY: `cwasm` was produced by `engine.precompile_component` in
        // this call.
        from_wasmtime(unsafe { Component::deserialize(engine, &cwasm) })
            .with_context(|| format!("failed to deserialize freshly compiled component for `{package_id}`"))
    }

    /// Compute the on-disk path for the cache entry keyed by `artifact_sha256`.
    fn cache_path(&self, artifact_sha256: &str) -> PathBuf {
        let key = compute_cache_key(artifact_sha256);
        self.dir.join(format!("{key}.cwasm"))
    }

    /// Remove `.cwasm` files whose mtime is older than `CACHE_MAX_AGE_DAYS`.
    fn evict_old_entries(&self) -> Result<()> {
        let max_age = std::time::Duration::from_secs(CACHE_MAX_AGE_DAYS * 86_400);
        let now = std::time::SystemTime::now();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("cwasm") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if now.duration_since(modified).unwrap_or_default() > max_age {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Convert a `wasmtime::Error` into `anyhow::Error`.
///
/// `wasmtime::Error` does not implement `std::error::Error` (it uses a custom
/// error type), so we cannot call `.with_context()` on it directly.  This
/// adapter maps it into an `anyhow::Error` so the standard context-chaining
/// combinators work.
fn from_wasmtime<T>(result: std::result::Result<T, wasmtime::Error>) -> Result<T> {
    result.map_err(anyhow::Error::from)
}

/// Compute a cache-entry filename from the four key axes.
fn compute_cache_key(artifact_sha256: &str) -> String {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let mut hasher = Sha256::new();
    hasher.update(artifact_sha256.as_bytes());
    hasher.update(b"|");
    hasher.update(WASMTIME_VERSION.as_bytes());
    hasher.update(b"|");
    hasher.update(ENGINE_CONFIG_KEY.as_bytes());
    hasher.update(b"|");
    hasher.update(target.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Write `bytes` to `dest` atomically by writing to a sibling temp file then
/// renaming.  On POSIX, rename(2) is atomic for same-filesystem paths; both
/// the dest and the temp file are in `self.dir` so they share the filesystem.
///
/// If another writer races us, both files contain valid precompiled output and
/// either winner is correct.
fn write_atomically(dest: &Path, bytes: &[u8]) -> Result<()> {
    let dir = dest.parent().context("cache path has no parent directory")?;
    let tmp = tempfile::Builder::new()
        .suffix(".cwasm.tmp")
        .tempfile_in(dir)
        .context("failed to create temporary file for atomic .cwasm write")?;
    fs::write(tmp.path(), bytes)
        .with_context(|| format!("failed to write .cwasm bytes to {}", tmp.path().display()))?;
    tmp.persist(dest).map(|_| ()).or_else(|e| {
        // On Windows, `persist` can fail if another writer raced us and
        // already placed the file.  If the dest now exists, treat it as a
        // concurrent write success.
        if dest.exists() {
            Ok(())
        } else {
            Err(e.error).context("failed to persist .cwasm temp file")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wasmtime::Config;

    fn test_engine() -> Engine {
        let mut config = Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).expect("create test engine")
    }

    fn sha256_hex_bytes(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        digest.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    #[test]
    fn cache_key_is_stable() {
        let key = compute_cache_key("abc123");
        assert_eq!(key.len(), 64, "cache key must be a 64-char hex SHA-256");
        // Same inputs → same key
        assert_eq!(key, compute_cache_key("abc123"));
        // Different artifact → different key
        assert_ne!(key, compute_cache_key("def456"));
    }

    #[test]
    fn write_atomically_creates_file() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("out.cwasm");
        write_atomically(&dest, b"hello").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn write_atomically_is_idempotent_when_dest_exists() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("out.cwasm");
        write_atomically(&dest, b"first").unwrap();
        // Second write should succeed (concurrent-write simulation)
        write_atomically(&dest, b"second").unwrap();
        // dest exists (either content is acceptable)
        assert!(dest.exists());
    }

    #[test]
    fn cache_path_differs_per_artifact_sha256() {
        let tmp = tempdir().unwrap();
        let cache = ComponentAotCache::open(tmp.path()).unwrap();
        let p1 = cache.cache_path("aaaa");
        let p2 = cache.cache_path("bbbb");
        assert_ne!(p1, p2);
    }

    #[test]
    fn open_creates_directory_if_absent() {
        let tmp = tempdir().unwrap();
        let cache_dir = tmp.path().join("nested/cache");
        assert!(!cache_dir.exists());
        ComponentAotCache::open(&cache_dir).unwrap();
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn cache_miss_writes_cwasm_file() {
        let engine = test_engine();
        let component_bytes = wat::parse_str("(component)").expect("parse minimal component");
        let sha256 = sha256_hex_bytes(&component_bytes);

        let tmp = tempdir().unwrap();
        let cache = ComponentAotCache::open(tmp.path().join("cache")).unwrap();

        let path = cache.cache_path(&sha256);
        assert!(!path.exists(), ".cwasm must not exist before first load");
        cache
            .load_or_compile(&engine, "test-pkg", &component_bytes, &sha256)
            .expect("load_or_compile on cache miss");
        assert!(path.exists(), ".cwasm must be written after cache miss");
    }

    #[test]
    fn cache_hit_returns_component_without_recompiling() {
        let engine = test_engine();
        let component_bytes = wat::parse_str("(component)").expect("parse minimal component");
        let sha256 = sha256_hex_bytes(&component_bytes);

        let tmp = tempdir().unwrap();
        let cache = ComponentAotCache::open(tmp.path().join("cache")).unwrap();

        // Warm the cache
        cache
            .load_or_compile(&engine, "test-pkg", &component_bytes, &sha256)
            .expect("first load: cache miss");

        let path = cache.cache_path(&sha256);
        let mtime_after_first = path.metadata().unwrap().modified().unwrap();

        // Hit the cache — the file should not be modified
        cache
            .load_or_compile(&engine, "test-pkg", &component_bytes, &sha256)
            .expect("second load: cache hit");

        let mtime_after_second = path.metadata().unwrap().modified().unwrap();
        assert_eq!(
            mtime_after_first, mtime_after_second,
            "cache file must not be rewritten on a hit"
        );
    }

    #[test]
    fn corrupted_cache_entry_is_rebuilt() {
        let engine = test_engine();
        let component_bytes = wat::parse_str("(component)").expect("parse minimal component");
        let sha256 = sha256_hex_bytes(&component_bytes);

        let tmp = tempdir().unwrap();
        let cache = ComponentAotCache::open(tmp.path().join("cache")).unwrap();

        // Warm the cache, then corrupt the entry
        cache
            .load_or_compile(&engine, "test-pkg", &component_bytes, &sha256)
            .expect("first load");
        let path = cache.cache_path(&sha256);
        fs::write(&path, b"not a valid .cwasm file").unwrap();

        // Should succeed by falling back to recompile
        cache
            .load_or_compile(&engine, "test-pkg", &component_bytes, &sha256)
            .expect("load after corruption must recompile");

        // The entry must have been replaced with a valid file
        assert!(path.exists());
        assert_ne!(fs::read(&path).unwrap(), b"not a valid .cwasm file");
    }

    // --- cache path resolution tests ---

    #[test]
    fn default_cache_dir_env_override_is_respected() {
        // Use a unique env-var value to avoid test cross-contamination.
        let tmp = tempdir().unwrap();
        let expected = tmp.path().join("ci-cache");
        // SAFETY: single-threaded test; no other thread reads this env var.
        unsafe { std::env::set_var(CACHE_DIR_ENV_VAR, expected.to_str().unwrap()) };
        let result = default_cache_dir();
        // SAFETY: single-threaded test; restoring env var.
        unsafe { std::env::remove_var(CACHE_DIR_ENV_VAR) };
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn default_cache_dir_empty_env_falls_back_to_platform() {
        // An empty value must be treated as unset and fall through to the platform dir.
        // SAFETY: single-threaded test; no other thread reads this env var.
        unsafe { std::env::set_var(CACHE_DIR_ENV_VAR, "") };
        let result = default_cache_dir();
        // SAFETY: single-threaded test; restoring env var.
        unsafe { std::env::remove_var(CACHE_DIR_ENV_VAR) };
        // We can't assert the exact path (platform-dependent) but it must either
        // be None (no home dir in some CI environments) or contain "checkleft".
        if let Some(p) = result {
            let s = p.to_string_lossy();
            assert!(
                s.contains("checkleft"),
                "platform cache dir should contain 'checkleft', got: {s}"
            );
        }
    }

    #[test]
    fn default_cache_dir_platform_path_contains_checkleft() {
        // If CACHE_DIR_ENV_VAR is not set, the resolved path (when available)
        // should be under the platform cache dir and contain "checkleft".
        // This test avoids mutating env state for the common case.
        if std::env::var(CACHE_DIR_ENV_VAR).is_ok() {
            return; // skip if another test left the var set
        }
        if let Some(p) = default_cache_dir() {
            let s = p.to_string_lossy();
            assert!(
                s.contains("checkleft"),
                "platform cache dir should contain 'checkleft', got: {s}"
            );
        }
        // None is also acceptable in headless CI with no home dir.
    }

    // --- cache round-trip from relocated dir test ---

    #[test]
    fn cache_round_trip_from_relocated_dir() {
        // Verify that a cache opened in an arbitrary non-repo directory (as
        // produced by default_cache_dir() or the env override) correctly
        // persists and reloads a compiled component.
        let engine = test_engine();
        let component_bytes = wat::parse_str("(component)").expect("parse minimal component");
        let sha256 = sha256_hex_bytes(&component_bytes);

        // Simulate a relocated, non-repo cache directory.
        let tmp = tempdir().unwrap();
        let relocated = tmp.path().join("relocated/cwasm");

        let cache = ComponentAotCache::open(&relocated).expect("open relocated cache dir");
        assert!(relocated.is_dir(), "open must create the directory");

        // Cold load — writes the .cwasm file.
        cache
            .load_or_compile(&engine, "relocate-test", &component_bytes, &sha256)
            .expect("cache miss: compile and write");

        let cwasm_path = cache.cache_path(&sha256);
        assert!(cwasm_path.exists(), ".cwasm must be written to relocated dir");
        assert!(
            cwasm_path.starts_with(&relocated),
            ".cwasm must be inside the relocated dir, got: {}",
            cwasm_path.display()
        );

        // Open a fresh handle to the same directory — simulates a second invocation.
        let cache2 = ComponentAotCache::open(&relocated).expect("re-open relocated cache dir");
        let mtime_before = cwasm_path.metadata().unwrap().modified().unwrap();

        // Warm load — must return without rewriting the file.
        cache2
            .load_or_compile(&engine, "relocate-test", &component_bytes, &sha256)
            .expect("cache hit: load from relocated dir");

        let mtime_after = cwasm_path.metadata().unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "cache hit must not rewrite the .cwasm file");
    }

    #[test]
    fn eviction_removes_old_cwasm_files() {
        let tmp = tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        // Plant a stale .cwasm file with mtime in the distant past.
        let stale = cache_dir.join("stale0000.cwasm");
        fs::write(&stale, b"old").unwrap();
        // Set mtime to 100 days ago (well past CACHE_MAX_AGE_DAYS = 90).
        let old_time = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(100 * 86_400))
            .unwrap();
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old_time)).unwrap();

        // Plant a fresh .cwasm file that should survive.
        let fresh = cache_dir.join("fresh0000.cwasm");
        fs::write(&fresh, b"new").unwrap();

        // Opening the cache triggers eviction.
        ComponentAotCache::open(&cache_dir).unwrap();

        assert!(!stale.exists(), "old entry must be evicted");
        assert!(fresh.exists(), "fresh entry must survive");
    }
}
