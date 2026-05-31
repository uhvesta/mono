//! Wrapper distribution — push, atomic-replace, version handshake.
//!
//! Phase 3 of the distributed-agent-execution design. Owns the
//! engine's contract for getting `boss-remote-run` onto a remote host
//! and keeping it current. Implements both the eager push at
//! `bossctl hosts add` and the lazy version-handshake at dispatch.
//!
//! Push sequence (per the design's "Atomic replace"):
//!
//! 1. `ssh remote 'mkdir -p ~/.boss-remote/bin'`
//! 2. `scp <local-tmpfile> remote:~/.boss-remote/bin/boss-remote-run.new`
//! 3. `ssh remote 'chmod 0755 ~/.boss-remote/bin/boss-remote-run.new
//!     && mv ~/.boss-remote/bin/boss-remote-run.new ~/.boss-remote/bin/boss-remote-run'`
//!
//! Concurrent dispatches on the same host serialize on a per-host
//! push lock so two flows never race on the `.new` filename. (The
//! lock is held only for the lifetime of one push; a long-running
//! worker that already saw a matching version never grabs it.)

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::remote_wrapper::{
    REMOTE_WRAPPER_DIR, REMOTE_WRAPPER_NAME, expected_version, remote_wrapper_path,
    rendered_wrapper,
};
use crate::ssh_transport::{SshFailureKind, SshTransport, classify_stderr};

/// Per-host push-lock registry. Each host id maps to one
/// `tokio::sync::Mutex<()>`; acquiring it before any push or
/// verify-then-push sequence serializes concurrent dispatches against
/// the same host without blocking dispatches to different hosts.
///
/// The registry itself uses a `std::sync::Mutex` for the map — only
/// held briefly to look up or insert an `Arc`; never held across an
/// `.await`.
#[derive(Debug, Default)]
pub struct WrapperPushLocks {
    inner: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WrapperPushLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the async mutex for `host_id`, creating one on first use.
    /// Callers that hold the returned lock guard serialize pushes to
    /// that host; callers for a different host get an independent lock.
    pub fn lock_for(&self, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.inner.lock().expect("WrapperPushLocks map poisoned");
        Arc::clone(
            map.entry(host_id.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

/// Outcome of a wrapper push. The engine surfaces these on the host
/// row's `last_error_text` and uses them to decide retry posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperPushOutcome {
    /// Wrapper pushed and `--version` returned the expected string.
    Ok,
    /// Push reached the host but the file could not be written. The
    /// `SshFailureKind` carries the sub-classification.
    Failed(SshFailureKind, String),
}

impl WrapperPushOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, WrapperPushOutcome::Ok)
    }
}

/// Push the wrapper to the remote host atomically. Verifies with
/// `--version` after the rename so a transport that returned 0 but
/// dropped bytes is still surfaced as a mismatch.
pub async fn push_wrapper(transport: &SshTransport) -> Result<WrapperPushOutcome> {
    // 1. Make sure the install dir exists. `mkdir -p` is idempotent
    //    and never fails on an existing directory.
    let mkdir_dir = format!("~/{REMOTE_WRAPPER_DIR}");
    let mkdir = transport
        .run(&["mkdir", "-p", mkdir_dir.as_str()])
        .await
        .with_context(|| format!("mkdir on host {}", transport.host_id))?;
    if !mkdir.success() {
        let kind = classify_stderr(&mkdir.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, mkdir.stderr));
    }

    // 2. Write the rendered wrapper to a local on-disk file so scp
    //    has a real path to push. Flush + close before scp opens it
    //    so the bytes are durable on disk.
    let local_path = materialize_wrapper_to_disk()?;
    let _local_path_guard = TempFileGuard(local_path.clone());

    // 3. scp to the `.new` filename. A concurrent dispatch on the same
    //    host that races into this code would race on the same `.new`
    //    filename — the per-host push lock (held by the caller) keeps
    //    that from happening.
    let remote_new = format!("{}/{REMOTE_WRAPPER_NAME}.new", expand_remote_dir());
    let push = transport
        .scp_push(&local_path, &remote_new)
        .await
        .with_context(|| format!("scp push to host {}", transport.host_id))?;
    if !push.success() {
        let kind = classify_stderr(&push.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, push.stderr));
    }

    // 4. Atomic rename + chmod 0755 in one round-trip. POSIX rename(2)
    //    on the same filesystem is atomic; concurrent dispatches see
    //    either the old or the new wrapper, never a half-written file.
    let remote_final = remote_wrapper_path();
    let chmod_script = format!(
        "chmod 0755 {dir}/{name}.new && mv {dir}/{name}.new {final_}",
        dir = expand_remote_dir(),
        name = REMOTE_WRAPPER_NAME,
        final_ = remote_final
    );
    let chmod = transport
        .run(&["sh", "-c", chmod_script.as_str()])
        .await
        .with_context(|| format!("chmod+mv on host {}", transport.host_id))?;
    if !chmod.success() {
        let kind = classify_stderr(&chmod.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, chmod.stderr));
    }

    // 5. Confirm with --version. A transport that succeeded but
    //    truncated bytes surfaces here as a version mismatch rather
    //    than a silent half-install.
    match verify_wrapper_version(transport).await? {
        VersionCheck::Match => Ok(WrapperPushOutcome::Ok),
        VersionCheck::Mismatch { actual } => Ok(WrapperPushOutcome::Failed(
            SshFailureKind::Unclassified,
            format!(
                "post-push version handshake mismatch: expected {} got {actual}",
                expected_version()
            ),
        )),
        VersionCheck::Missing => Ok(WrapperPushOutcome::Failed(
            SshFailureKind::Unclassified,
            "wrapper missing after push (--version returned non-zero)".to_owned(),
        )),
    }
}

/// Outcome of a `--version` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// `--version` returned the engine's expected version.
    Match,
    /// `--version` returned a different version. Trigger re-push.
    Mismatch { actual: String },
    /// The wrapper file is absent or unexecutable. Trigger push.
    Missing,
}

/// Invoke the wrapper with `--version` over the existing master and
/// compare to [`expected_version`]. Per the design: exact-equality, no
/// semver.
pub async fn verify_wrapper_version(transport: &SshTransport) -> Result<VersionCheck> {
    let wrapper_path = remote_wrapper_path();
    let output = transport
        .run(&[wrapper_path.as_str(), "--version"])
        .await
        .with_context(|| format!("--version probe on host {}", transport.host_id))?;
    if !output.success() {
        return Ok(VersionCheck::Missing);
    }
    let actual = output.stdout.trim().to_owned();
    if actual == expected_version() {
        Ok(VersionCheck::Match)
    } else {
        Ok(VersionCheck::Mismatch { actual })
    }
}

/// Convenience: push the wrapper unconditionally if `--version`
/// reports anything other than [`VersionCheck::Match`]. Returns the
/// push outcome (or [`WrapperPushOutcome::Ok`] when the handshake
/// already matched).
///
/// Acquires the per-host lock from `locks` before running
/// verify-then-push so the pair is atomic against concurrent callers
/// on the same host. Callers for different hosts run in parallel.
pub async fn ensure_wrapper_current(
    transport: &SshTransport,
    locks: &WrapperPushLocks,
) -> Result<WrapperPushOutcome> {
    let host_lock = locks.lock_for(&transport.host_id);
    let _guard = host_lock.lock().await;
    match verify_wrapper_version(transport).await? {
        VersionCheck::Match => Ok(WrapperPushOutcome::Ok),
        VersionCheck::Mismatch { .. } | VersionCheck::Missing => push_wrapper(transport).await,
    }
}

/// Path used in remote shell commands. Just `~/.boss-remote/bin`;
/// kept in one place so the design's tweak from `~/.local/bin` is
/// rooted in `REMOTE_WRAPPER_DIR` and not duplicated across modules.
fn expand_remote_dir() -> String {
    format!("~/{REMOTE_WRAPPER_DIR}")
}

/// Run-failure-reason string for the design's `host_wrapper_push_failed`.
/// Stored verbatim on the `work_runs.error_text` and surfaced as an
/// attention item; the sub-classification goes into `last_error_text`.
pub const RUN_FAILURE_REASON_WRAPPER_PUSH_FAILED: &str = "host_wrapper_push_failed";

/// Human-readable subcategory shorthand used on `hosts.last_error_text`
/// when a push fails. Matches the design's verbatim labels so docs and
/// code stay in sync.
pub fn subclass_label(kind: &SshFailureKind) -> &'static str {
    match kind {
        SshFailureKind::DiskFull => "disk_full",
        SshFailureKind::PermissionDenied => "permission_denied",
        SshFailureKind::ConnectionLost => "connection_lost",
        SshFailureKind::Unclassified => "unclassified",
    }
}

/// Materialize the rendered wrapper to a stable on-disk path so scp
/// can read it. Production path uses [`TempFileGuard`] to clean up on
/// drop; tests call it directly and unlink the file themselves.
pub fn materialize_wrapper_to_disk() -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("boss-remote-run.{}.{}.sh", std::process::id(), suffix));
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("creating wrapper staging file at {path:?}"))?;
    file.write_all(rendered_wrapper().as_bytes())
        .with_context(|| format!("writing wrapper bytes to {path:?}"))?;
    file.flush().with_context(|| format!("flushing {path:?}"))?;
    Ok(path)
}

/// RAII helper to unlink the local staging file when the push flow
/// goes out of scope. Errors during unlink are logged but not
/// propagated — leaking a few-hundred-byte file in `$TMPDIR` is
/// strictly better than masking the actual push error in the result.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    ?err,
                    path = %self.0.display(),
                    "wrapper_distribution: failed to unlink local staging file"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── WrapperPushLocks tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn same_host_locks_contend() {
        // Two callers on the same host id must see the same mutex, so
        // they serialize: while one guard is held, try_lock on a second
        // handle fails.
        let locks = WrapperPushLocks::new();
        let lock1 = locks.lock_for("host-a");
        let lock2 = locks.lock_for("host-a");

        let _guard = lock1.lock().await;
        assert!(
            lock2.try_lock().is_err(),
            "same-host second lock_for should contend while first guard is held"
        );
    }

    #[tokio::test]
    async fn same_host_serializes_after_release() {
        // Once the first guard is dropped the second caller can proceed.
        let locks = WrapperPushLocks::new();
        let lock1 = locks.lock_for("host-a");
        let lock2 = locks.lock_for("host-a");

        {
            let _guard = lock1.lock().await;
            assert!(lock2.try_lock().is_err());
            // _guard dropped here
        }
        assert!(
            lock2.try_lock().is_ok(),
            "same-host lock should be acquirable after first guard is released"
        );
    }

    #[tokio::test]
    async fn different_hosts_do_not_contend() {
        // Holding host-a's lock must not block host-b.
        let locks = WrapperPushLocks::new();
        let lock_a = locks.lock_for("host-a");
        let lock_b = locks.lock_for("host-b");

        let _guard_a = lock_a.lock().await;
        assert!(
            lock_b.try_lock().is_ok(),
            "different-host locks should be independent"
        );
    }

    #[test]
    fn lock_for_returns_identical_arc_for_same_host() {
        // The registry must hand out the *same* Arc for repeated calls
        // with the same host id so that two callers contend on one mutex.
        let locks = WrapperPushLocks::new();
        let a = locks.lock_for("zakalwe");
        let b = locks.lock_for("zakalwe");
        assert!(
            Arc::ptr_eq(&a, &b),
            "lock_for must return the same Arc for the same host id"
        );
    }

    #[test]
    fn lock_for_returns_distinct_arcs_for_different_hosts() {
        let locks = WrapperPushLocks::new();
        let a = locks.lock_for("host-a");
        let b = locks.lock_for("host-b");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "lock_for must return different Arcs for different host ids"
        );
    }

    #[test]
    fn subclass_labels_match_design() {
        assert_eq!(subclass_label(&SshFailureKind::DiskFull), "disk_full");
        assert_eq!(subclass_label(&SshFailureKind::PermissionDenied), "permission_denied");
        assert_eq!(subclass_label(&SshFailureKind::ConnectionLost), "connection_lost");
        assert_eq!(subclass_label(&SshFailureKind::Unclassified), "unclassified");
    }

    #[test]
    fn wrapper_push_outcome_is_ok_only_when_ok() {
        assert!(WrapperPushOutcome::Ok.is_ok());
        assert!(!WrapperPushOutcome::Failed(SshFailureKind::DiskFull, "x".into()).is_ok());
    }

    #[test]
    fn materialize_round_trips_with_version_stamp() {
        let path = materialize_wrapper_to_disk().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Rendered wrapper has the version stamp baked in.
        assert!(
            text.contains(&expected_version()),
            "staging file should contain `{}` but did not\n{text}",
            expected_version()
        );
        assert!(text.starts_with("#!/bin/sh\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_failure_reason_constant_matches_design() {
        // The design names the reason verbatim; this constant must
        // match so attention items / failure tables can be filtered
        // by string compare.
        assert_eq!(
            RUN_FAILURE_REASON_WRAPPER_PUSH_FAILED,
            "host_wrapper_push_failed"
        );
    }
}
