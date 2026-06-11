//! Regression test: `boss uninstall` with BOSS_INSTALL_ROOT set must NOT
//! stop the host engine.
//!
//! The pre-fix bug: the uninstall handler called stop_engine against the
//! default /tmp/boss-engine.pid regardless of BOSS_INSTALL_ROOT. When a
//! worker (or a developer) ran `boss uninstall --yes` against a sandbox
//! install root, the live host engine was killed.
//!
//! The fix: stop_engine is only called when BOSS_INSTALL_ROOT is absent
//! (i.e. the default ~/Applications install root is in use). When
//! BOSS_INSTALL_ROOT is set the caller owns their engine lifecycle.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn boss_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_boss") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    if let Ok(runfiles_dir) = std::env::var("RUNFILES_DIR") {
        let p = PathBuf::from(runfiles_dir).join("_main/tools/boss/cli/boss");
        if p.exists() {
            return p;
        }
    }
    panic!("boss binary not found; compile with cargo test or bazel test");
}

/// Regression: `boss uninstall --yes` with BOSS_INSTALL_ROOT set must not
/// send SIGTERM to the process whose PID lives in BOSS_ENGINE_PID_PATH.
///
/// Test strategy:
/// 1. Create a sandbox install root with a fake Boss.app inside.
/// 2. Spawn a long-lived dummy process (`sleep 3600`) that stands in for
///    the "host engine".
/// 3. Write its PID to a test-local file and point BOSS_ENGINE_PID_PATH
///    at it (so we don't touch /tmp/boss-engine.pid on the real host).
/// 4. Run `boss uninstall --yes` with BOSS_INSTALL_ROOT set.
/// 5. Assert the dummy is still alive — stop_engine must not have fired.
#[test]
fn sandbox_uninstall_does_not_kill_dummy_engine() {
    let install_root = tempfile::tempdir().expect("create install root tempdir");
    let pid_dir = tempfile::tempdir().expect("create pid tempdir");

    // Fake Boss.app must exist so the uninstall path doesn't bail early.
    let app_path = install_root.path().join("Boss.app");
    std::fs::create_dir_all(&app_path).expect("create fake Boss.app dir");

    // Spawn a long-running dummy that we treat as the "host engine".
    let mut dummy = Command::new("sleep")
        .arg("3600")
        .spawn()
        .expect("failed to spawn dummy sleep process");
    let dummy_pid = dummy.id();

    // Write PID to a test-local file (not /tmp/boss-engine.pid, to avoid
    // interfering with any real running host engine on this machine).
    let pid_file = pid_dir.path().join("boss-engine.pid");
    std::fs::write(&pid_file, dummy_pid.to_string()).expect("write pid file");

    let _output = Command::new(boss_binary())
        .args(["uninstall", "--yes"])
        .env("BOSS_INSTALL_ROOT", install_root.path())
        .env("BOSS_ENGINE_PID_PATH", &pid_file)
        // Prevent any incidental autostart attempt.
        .env("BOSS_ENGINE_CMD", "false")
        .output()
        .expect("failed to exec boss uninstall");

    // Give SIGTERM time to propagate if stop_engine was (incorrectly) called.
    std::thread::sleep(Duration::from_millis(200));

    let still_running = dummy.try_wait().expect("try_wait on dummy process").is_none();

    // Cleanup before asserting so the dummy is always reaped.
    dummy.kill().ok();
    dummy.wait().ok();

    assert!(
        still_running,
        "dummy process (pid {dummy_pid}) was killed by `boss uninstall --yes` \
         even though BOSS_INSTALL_ROOT was set to a sandbox path. \
         This is the pre-fix bug: stop_engine must be skipped when \
         BOSS_INSTALL_ROOT is set."
    );
}
