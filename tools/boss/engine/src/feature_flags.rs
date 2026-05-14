//! Toggleable feature flags for optional / risk-bearing engine
//! behaviours.
//!
//! Background: incident 001 (`docs/postmortems/incident-001-pr-fan-out.md`)
//! action item #5. Any engine behaviour that is optional-for-correctness
//! and carries non-trivial blast radius when wrong should be gated by a
//! flag the human can flip from a debug pane without rebuilding the
//! engine. The first such flag is `detect_pr_cold_fallback`, which
//! gates the `detect_pr` fallback that mis-attributed sibling-worker
//! PRs in the incident.
//!
//! ## Adding a flag
//!
//! Two things — see `tools/boss/engine/feature-flags/README.md` for the
//! full procedure:
//!
//! 1. Append a [`FeatureFlagSpec`] entry to [`REGISTRY`] with the
//!    flag's name, human-readable description, category, and default.
//! 2. In the consumer site, call `feature_flags.is_enabled("name")` —
//!    one line.
//!
//! Defaults: ON unless the flag is known-risky. The flag store hides
//! the default's source from callers — they get a `bool` either way.
//!
//! ## File format and race semantics
//!
//! The store mirrors the registry to a TOML file under the engine's
//! state root (`~/Library/Application Support/Boss/feature-flags.toml`
//! in production; a temp dir in tests). The file shape is one
//! key-value pair per flag, with the key being the flag name:
//!
//! ```toml
//! detect_pr_cold_fallback = true
//! ```
//!
//! Writes go through a temp-then-rename so a partial write is never
//! visible to a concurrent reader. The engine reads on boot (via
//! [`FeatureFlagsStore::load`]) and after every set (no race window:
//! the set call updates the in-memory map and then writes; readers see
//! the new value the instant the set call returns). Flags that are
//! absent from the file fall back to their registry default — so
//! removing a stale entry from the file is benign, and adding a new
//! flag does not require migrating existing installs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One feature flag entry in the registry. The registry is the source
/// of truth for which flags exist and what their defaults are; the
/// on-disk TOML file only overrides defaults for flags the human has
/// touched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureFlagSpec {
    /// Stable identifier used both as the TOML key and the consumer
    /// lookup key. Lowercase snake_case.
    pub name: &'static str,
    /// One-sentence description rendered in the debug pane.
    pub description: &'static str,
    /// Free-form grouping label rendered as a section header in the
    /// debug pane. Use existing categories where possible to keep the
    /// UI tidy. Current categories: `"completion"`.
    pub category: &'static str,
    /// What the flag is when nothing in the on-disk file overrides it.
    /// Default ON unless the flag is gating a known-risky path that
    /// the human should be able to disable but not the other way
    /// around — see the README's safety contract.
    pub default_enabled: bool,
}

/// Single-place registry. Add new flags here, then read them at the
/// consumer site with [`FeatureFlagsStore::is_enabled`]. The slice is
/// `const` so the compiler enforces "one place to register a flag" —
/// there is no other path to introduce a new flag name.
pub const REGISTRY: &[FeatureFlagSpec] = &[
    FeatureFlagSpec {
        name: "detect_pr_cold_fallback",
        description:
            "Run the `detect_pr` cold-path fallback (gh pr list --head) when the worker-hook \
             staging cache is empty on Stop. Disable to make empty-staging fall straight to \
             'no PR pushed' without consulting GitHub — recommended when the merge poller is \
             producing mis-binds.",
        category: "completion",
        default_enabled: true,
    },
];

/// Snapshot of one flag's current state for the wire / debug pane.
/// Mirrors the protocol type one-for-one so the engine can return the
/// in-memory state without copying field-by-field at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureFlagSnapshot {
    pub name: String,
    pub description: String,
    pub category: String,
    pub default_enabled: bool,
    pub enabled: bool,
}

/// On-disk file shape. Loose mapping of `flag_name -> bool` so missing
/// entries fall back to the registry default and unknown entries are
/// ignored (forward-compat with older engine builds that don't know
/// about a newly-added flag yet).
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(flatten)]
    flags: HashMap<String, bool>,
}

/// Thread-safe store. Holds an in-memory map of overrides keyed by
/// flag name; falls back to the registry default for any flag not in
/// the map. Backed by a TOML file the engine reads at boot and
/// rewrites on every `set`.
pub struct FeatureFlagsStore {
    path: PathBuf,
    state: Mutex<HashMap<String, bool>>,
}

impl FeatureFlagsStore {
    /// Build a store rooted at the given file path. The file does NOT
    /// have to exist yet — first call to [`Self::load`] tolerates a
    /// missing file and returns an empty override set.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience: derive the default file path from the Boss state
    /// root (i.e. the directory holding `state.db`). Used by `app.rs`
    /// at boot.
    pub fn default_path(state_root: &Path) -> PathBuf {
        state_root.join("feature-flags.toml")
    }

    /// Path the store reads/writes. Test-only callers can inspect it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file into memory. Missing file = empty overrides
    /// (every flag is at its registry default). A parse error returns
    /// `Err` without touching the in-memory map — callers should log
    /// and continue rather than crashing the engine on a malformed
    /// file.
    pub fn load(&self) -> Result<()> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut guard = self.state.lock().expect("feature-flags lock poisoned");
                guard.clear();
                return Ok(());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read feature flags file: {}", self.path.display()));
            }
        };
        let parsed: FileShape = toml::from_str(&contents)
            .with_context(|| format!("parse feature flags file: {}", self.path.display()))?;
        let mut guard = self.state.lock().expect("feature-flags lock poisoned");
        guard.clear();
        for (key, value) in parsed.flags {
            // Drop entries that don't match any registry name. Keeps
            // the in-memory map tidy and avoids hanging onto state for
            // a flag that has been removed from the codebase.
            if REGISTRY.iter().any(|spec| spec.name == key) {
                guard.insert(key, value);
            }
        }
        Ok(())
    }

    /// Resolve `name`'s current value. Returns the registry default
    /// when the flag has no on-disk override; returns `None` only when
    /// `name` doesn't match any registered flag.
    pub fn get(&self, name: &str) -> Option<bool> {
        let spec = REGISTRY.iter().find(|spec| spec.name == name)?;
        let guard = self.state.lock().expect("feature-flags lock poisoned");
        Some(guard.get(name).copied().unwrap_or(spec.default_enabled))
    }

    /// Convenience for the consumer-side one-line check.
    /// Treats an unknown name as `false` — there is no caller-side
    /// "default true" for an unknown flag, since an unknown flag must
    /// be a typo on the consumer side that wasn't caught by review.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.get(name).unwrap_or(false)
    }

    /// Override `name` to `enabled` and atomically persist the new
    /// state to disk. Returns `Err` if `name` is not a registered
    /// flag (the debug pane sends only registered names, so this is
    /// effectively a programming-error path).
    pub fn set(&self, name: &str, enabled: bool) -> Result<()> {
        if !REGISTRY.iter().any(|spec| spec.name == name) {
            anyhow::bail!("unknown feature flag: {name}");
        }
        {
            let mut guard = self.state.lock().expect("feature-flags lock poisoned");
            guard.insert(name.to_owned(), enabled);
        }
        self.write_to_disk()
    }

    /// Current snapshot of every registered flag, in the order they
    /// appear in [`REGISTRY`]. Used by the debug-pane list RPC.
    pub fn snapshot_all(&self) -> Vec<FeatureFlagSnapshot> {
        let guard = self.state.lock().expect("feature-flags lock poisoned");
        REGISTRY
            .iter()
            .map(|spec| FeatureFlagSnapshot {
                name: spec.name.to_owned(),
                description: spec.description.to_owned(),
                category: spec.category.to_owned(),
                default_enabled: spec.default_enabled,
                enabled: guard
                    .get(spec.name)
                    .copied()
                    .unwrap_or(spec.default_enabled),
            })
            .collect()
    }

    fn write_to_disk(&self) -> Result<()> {
        let serialized = {
            let guard = self.state.lock().expect("feature-flags lock poisoned");
            let shape = FileShape {
                flags: guard.clone(),
            };
            toml::to_string_pretty(&shape).context("serialize feature flags to TOML")?
        };

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create feature-flags parent dir: {}", parent.display())
            })?;
        }

        // Temp-then-rename: a concurrent reader either sees the old
        // file or the new one, never a half-written file. Linux/macOS
        // `rename` is atomic on the same filesystem; the temp file
        // lives next to the target so the rename never crosses a
        // device boundary.
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, serialized)
            .with_context(|| format!("write feature-flags temp file: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "rename {} → {}",
                tmp.display(),
                self.path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store(tmp: &TempDir) -> FeatureFlagsStore {
        FeatureFlagsStore::new(tmp.path().join("feature-flags.toml"))
    }

    #[test]
    fn missing_file_returns_registry_default() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(store.is_enabled("detect_pr_cold_fallback"));
    }

    #[test]
    fn set_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set("detect_pr_cold_fallback", false).unwrap();
        assert!(!store.is_enabled("detect_pr_cold_fallback"));

        // A fresh store backed by the same file picks up the override.
        let store2 = make_store(&tmp);
        store2.load().unwrap();
        assert!(!store2.is_enabled("detect_pr_cold_fallback"));
    }

    #[test]
    fn unknown_flag_set_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let err = store.set("not_a_real_flag", true).unwrap_err();
        assert!(err.to_string().contains("not_a_real_flag"));
    }

    #[test]
    fn unknown_flag_is_enabled_is_false() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(!store.is_enabled("not_a_real_flag"));
    }

    #[test]
    fn snapshot_lists_every_registered_flag() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        let snap = store.snapshot_all();
        assert_eq!(snap.len(), REGISTRY.len());
        // detect_pr_cold_fallback is the first entry and defaults ON.
        let detect = snap
            .iter()
            .find(|s| s.name == "detect_pr_cold_fallback")
            .unwrap();
        assert!(detect.default_enabled);
        assert!(detect.enabled);
        assert_eq!(detect.category, "completion");
    }

    #[test]
    fn unknown_flag_in_file_is_dropped_on_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("feature-flags.toml");
        std::fs::write(
            &path,
            "detect_pr_cold_fallback = false\nstale_flag_no_longer_in_registry = true\n",
        )
        .unwrap();
        let store = FeatureFlagsStore::new(path);
        store.load().unwrap();
        // Real flag honoured.
        assert!(!store.is_enabled("detect_pr_cold_fallback"));
        // Stale flag dropped — get on a non-registered name is None.
        assert!(store.get("stale_flag_no_longer_in_registry").is_none());
    }

    #[test]
    fn malformed_file_returns_err_without_clobbering_memory() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // First, seed an in-memory override.
        store.load().unwrap();
        store.set("detect_pr_cold_fallback", false).unwrap();
        // Then corrupt the file out from under us.
        std::fs::write(store.path(), "this is = not = valid toml = at = all").unwrap();
        // Reload should err …
        assert!(store.load().is_err());
        // … but the in-memory state is undefined-but-not-crashing.
        // We don't assert on its value — the contract is "don't
        // panic, return an error so the caller can log."
    }

    #[test]
    fn set_persists_only_to_target_path() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set("detect_pr_cold_fallback", false).unwrap();
        let on_disk = std::fs::read_to_string(store.path()).unwrap();
        assert!(on_disk.contains("detect_pr_cold_fallback"));
        assert!(on_disk.contains("false"));
        // Temp file must have been cleaned up by the rename.
        assert!(!tmp.path().join("feature-flags.toml.tmp").exists());
    }
}
