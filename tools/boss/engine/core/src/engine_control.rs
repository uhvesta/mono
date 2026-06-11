//! Engine-control authentication token.
//!
//! The frontend socket sits on a well-known path that any process on
//! the same user/machine can dial. SIGTERM has the same property — any
//! caller with the right pid can land it, and the engine can't tell a
//! deliberate "the macOS app is auto-restarting me" signal apart from
//! a worker accidentally targeting `/tmp/boss-engine.pid`. Two
//! `bazel test`-mediated incidents in May 2026 killed the live engine
//! exactly that way (issue #705).
//!
//! This module owns the secret half of the proposed defense: a random
//! 32-byte token, written to a 0600 file under
//! `~/Library/Application Support/Boss/`. The `shutdown` RPC on the
//! frontend socket accepts the token and only the token; SIGTERM
//! becomes the fallback for OS-shutdown / panic paths rather than the
//! everyday "restart engine" gesture.
//!
//! The token file is the boundary the bazel sandbox already enforces:
//! `darwin-sandbox` denies test actions any access under
//! `~/Library/Application Support/`, so a test that ends up calling
//! the canonical-shutdown path reads `ENOENT`, fails auth, and the
//! live engine survives.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// File mode for the control-token file. The token is the auth
/// credential for the shutdown RPC, so the file must not be readable
/// by other users on multi-tenant macs.
const TOKEN_FILE_MODE: u32 = 0o600;

/// Optional override for the token path. Mirrors the pattern used
/// by `BOSS_ENGINE_PID_PATH` / `BOSS_ENGINE_AUDIT_PATH` so tests can
/// point this somewhere harmless.
pub const TOKEN_PATH_ENV: &str = "BOSS_ENGINE_CONTROL_TOKEN_PATH";

/// On-disk layout for the token file. Stored as JSON rather than raw
/// bytes so the file is self-describing — a future tool that needs to
/// reconcile "which engine does this token belong to?" has the socket
/// path right there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlTokenFile {
    /// Hex-encoded random bytes. The engine compares against this
    /// value verbatim; rotation is per-engine-start.
    pub token: String,
    /// The frontend socket path the engine bound on this run. A client
    /// that resolves the production path via `BossEnginePaths` and the
    /// token via this file can confirm they're talking to the engine
    /// that minted the token before sending the shutdown RPC.
    pub socket_path: String,
    /// Engine pid that minted the token. Diagnostic only — the RPC
    /// itself only validates the token string.
    pub pid: u32,
}

/// Default token location: alongside the other Boss state files under
/// `~/Library/Application Support/Boss/`. Honours
/// [`TOKEN_PATH_ENV`] first so a test instance can point this
/// elsewhere without inheriting the production path.
pub fn default_token_path() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(TOKEN_PATH_ENV) {
        let trimmed = override_path.to_string_lossy().trim().to_owned();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss/engine-control.token"))
}

/// Generate a fresh 32-byte hex-encoded token. Backed by
/// `fastrand::Rng::with_seed`-style RNG seeded from `getrandom` —
/// `fastrand` is already in the workspace deps, and 256 bits of
/// entropy via its `Rng::u64()` rolled four times is overkill for an
/// auth credential whose threat model is "the wrong test ended up in
/// the production codepath", not "a remote adversary."
pub fn generate_token() -> String {
    // Seed from OS entropy by way of `fastrand`'s default seeder
    // (`fastrand::Rng::new()` already uses a thread-local CSPRNG-ish
    // seed). Four u64 draws → 32 bytes → 64 hex chars.
    let mut rng = fastrand::Rng::new();
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        let word = rng.u64(..);
        let word_bytes = word.to_le_bytes();
        chunk.copy_from_slice(&word_bytes[..chunk.len()]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

/// Write the token file with mode 0600, creating parent directories
/// as needed. Overwrites any prior file at the path — a stale token
/// from a previous engine that crashed without removing its file
/// would otherwise authenticate against the wrong process.
pub fn write_token_file(path: &Path, contents: &ControlTokenFile) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create control-token directory {}", parent.display()))?;
    }

    let serialized = serde_json::to_string(contents).context("failed to serialize control-token file")?;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(TOKEN_FILE_MODE)
        .open(path)
        .with_context(|| format!("failed to open control-token file {}", path.display()))?;
    file.write_all(serialized.as_bytes())
        .with_context(|| format!("failed to write control-token file {}", path.display()))?;
    Ok(())
}

/// RAII guard that removes the token file when dropped — both on
/// graceful return and on panic-unwind. Only removes the file if it
/// still records the expected pid; another engine that won the bind
/// race wouldn't appreciate having its token swept by our drop.
pub struct ControlTokenGuard {
    path: PathBuf,
    pid: u32,
}

impl ControlTokenGuard {
    pub fn new(path: PathBuf, pid: u32) -> Self {
        Self { path, pid }
    }
}

impl Drop for ControlTokenGuard {
    fn drop(&mut self) {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<ControlTokenFile>(&raw) else {
            // Don't remove a file we can't parse — it might belong to
            // a future engine version.
            return;
        };
        if parsed.pid != self.pid {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_token_returns_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_not_constant() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two consecutive draws collided — RNG broken?");
    }

    #[test]
    fn write_token_file_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "deadbeef".into(),
            socket_path: "/tmp/boss-engine.sock".into(),
            pid: 12345,
        };
        write_token_file(&path, &contents).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "deadbeef");
        assert_eq!(parsed.socket_path, "/tmp/boss-engine.sock");
        assert_eq!(parsed.pid, 12345);
    }

    #[test]
    fn write_token_file_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/dir/engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 1,
        };
        write_token_file(&path, &contents).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn write_token_file_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 1,
        };
        write_token_file(&path, &contents).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn guard_removes_file_with_matching_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 99,
        };
        write_token_file(&path, &contents).unwrap();
        {
            let _guard = ControlTokenGuard::new(path.clone(), 99);
        }
        assert!(!path.exists(), "guard should remove the file");
    }

    #[test]
    fn guard_leaves_file_with_mismatched_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 12345,
        };
        write_token_file(&path, &contents).unwrap();
        {
            let _guard = ControlTokenGuard::new(path.clone(), 99);
        }
        assert!(path.exists(), "guard with mismatched pid must not remove the file");
    }

    #[test]
    fn default_token_path_honours_env_override() {
        let dir = TempDir::new().unwrap();
        let override_path = dir.path().join("override.token");
        // SAFETY: single-threaded test scope.
        unsafe {
            std::env::set_var(TOKEN_PATH_ENV, &override_path);
        }
        let resolved = default_token_path().unwrap();
        assert_eq!(resolved, override_path);
        unsafe {
            std::env::remove_var(TOKEN_PATH_ENV);
        }
    }
}
