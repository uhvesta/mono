//! Remote-spawn planning + transport seam (Phase 3 follow-up, PR 1 of
//! the remote-SSH-dispatch stack).
//!
//! This module holds the *pure, engine-collaborator-free* half of the
//! remote spawn: how the wrapper command is composed, where the
//! forwarded events socket lives on the remote, how the wrapper's
//! sentinel exit codes map onto the failure-reason taxonomy, and the
//! `ssh -O forward` orchestration that opens the events tunnel and
//! launches the (detached) worker.
//!
//! It deliberately does **not** know about `WorkDb`, `RuntimeConfig`,
//! the events-socket path, the worker registry, or the live-status
//! surface — wiring those into `SshHostAdapter::spawn_worker` is PR 2 of
//! the stack. Keeping this layer pure means the whole remote launch
//! path is exercised in-process against a stubbed transport
//! ([`SshExec`]), so CI never depends on a live remote.
//!
//! ## Why a [`SshExec`] seam
//!
//! `SshTransport` shells out to real `ssh`. To test the orchestration
//! (forward setup, command shape, exit-code mapping, forward teardown
//! on failure) we abstract just the three transport operations the
//! launch needs behind a trait that `SshTransport` implements and a
//! test fake also implements. The trait is intentionally narrow — it is
//! not a general transport abstraction, only the surface the spawn
//! orchestration touches.
//!
//! ## ControlMaster reuse
//!
//! [`SshExec::add_reverse_unix_forward`] / `cancel_reverse_unix_forward`
//! are `ssh -O forward` / `-O cancel` against the existing master, and
//! the worker launch is one more `ssh` over the same `ControlPath`. No
//! parallel SSH session is opened for events vs. exec — they share the
//! one multiplex, per the task's `ControlMaster`-reuse requirement.

use anyhow::Result;
use async_trait::async_trait;

use crate::ssh_transport::{SshOutput, SshTransport};

// ── Failure-reason taxonomy (matches the wrapper sentinels) ───────────────────

/// `exit 78` (EX_CONFIG): the wrapper's env contract was not satisfied
/// (missing var, workspace path absent). An engine bug, surfaced so it
/// is not silently swallowed.
pub const REASON_WRAPPER_MISCONFIGURED: &str = "host_wrapper_misconfigured";
/// `exit 79`: `claude` not found on the remote PATH.
pub const REASON_MISSING_CLAUDE: &str = "host_missing_claude";
/// `exit 80`: `cube` not found on the remote PATH.
pub const REASON_MISSING_CUBE: &str = "host_missing_cube";
/// `exit 81`: `gh` not found on the remote PATH.
pub const REASON_MISSING_GH: &str = "host_missing_gh";
/// Any other non-zero wrapper exit, or a failure to even establish the
/// events forward before launch.
pub const REASON_WORKER_LAUNCH_FAILED: &str = "worker_launch_failed";

/// Outcome of classifying the wrapper's exit status. `Launched` means
/// the wrapper validated its contract and backgrounded the worker; it
/// does **not** mean the worker finished (the worker runs detached and
/// its lifecycle is driven by hook events over the forwarded socket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperLaunch {
    /// Wrapper backgrounded the worker successfully (`exit 0`).
    Launched,
    /// Wrapper refused to launch; the `&'static str` is the
    /// failure-reason taxonomy string.
    Failed(&'static str),
}

/// Map a wrapper exit code onto the launch outcome. The sentinel codes
/// (78–81) are the wrapper's documented contract; anything else is an
/// opaque launch failure.
pub fn classify_wrapper_exit(code: i32) -> WrapperLaunch {
    match code {
        0 => WrapperLaunch::Launched,
        78 => WrapperLaunch::Failed(REASON_WRAPPER_MISCONFIGURED),
        79 => WrapperLaunch::Failed(REASON_MISSING_CLAUDE),
        80 => WrapperLaunch::Failed(REASON_MISSING_CUBE),
        81 => WrapperLaunch::Failed(REASON_MISSING_GH),
        _ => WrapperLaunch::Failed(REASON_WORKER_LAUNCH_FAILED),
    }
}

// ── Events socket path on the remote ──────────────────────────────────────────

/// Path of the forwarded events socket on the *remote* host. The engine
/// requests `ssh -R <this>:<engine events.sock>`; the wrapper hands this
/// path to the worker as `BOSS_EVENTS_SOCKET`, so the `boss-event` shim
/// writes to what looks like a local socket and the bytes tunnel back.
///
/// Keyed by run id so concurrent remote runs on the same host never
/// collide on the socket path. The run id is sanitized to the safe
/// filename charset (run ids are `exec_*` / `run_*`, already safe, but
/// we defend against anything else).
pub fn remote_events_socket_path(run_id: &str) -> String {
    format!("/tmp/boss-events-{}.sock", sanitize_run_id(run_id))
}

fn sanitize_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

// ── Remote command composition ────────────────────────────────────────────────

/// Everything needed to compose the `env … wrapper` argv the engine
/// runs on the remote. All fields are plain owned data — the caller
/// (PR 2's `spawn_worker`) fills them from the execution + lease.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct RemoteSpawnPlan {
    /// Engine-assigned run id. Spliced into every hook payload by the
    /// shim (`_boss_run_id`) for correlation.
    pub run_id: String,
    /// Cube lease id (already leased by the engine before launch).
    pub lease_id: String,
    /// Absolute workspace path on the remote.
    pub workspace_path: String,
    /// Repo origin URL (informational for the worker).
    pub repo_remote_url: Option<String>,
    /// Forwarded events-socket path on the remote (from
    /// [`remote_events_socket_path`]).
    pub events_socket_path: String,
    /// Remote path of a file holding the initial prompt, if the engine
    /// shipped one. Preferred over an inline env var so a multi-KB
    /// prompt never has to survive ssh-argv re-quoting.
    pub initial_input_file: Option<String>,
    /// Remote path of the worker's `--settings` JSON file — rendered by
    /// [`crate::worker_setup::render_remote_settings_json`] and shipped
    /// outside the workspace tree (mirroring the local runner, which
    /// keeps the settings file out of the repo so it never lands in a
    /// worker's PR). When present the wrapper passes `--settings <file>`
    /// to claude so the `boss-event` hooks fire and the Stop event
    /// tunnels back over the forwarded socket. `None` falls back to
    /// claude's own project/user settings discovery.
    pub settings_file: Option<String>,
    /// Absolute remote path of the wrapper (`~/.boss-remote/bin/boss-remote-run`).
    pub wrapper_path: String,
}

/// Compose the remote command as an argv vector: an `env VAR=val …`
/// prefix that sets the wrapper's contract, then the wrapper path. The
/// vector is logical tokens; the transport is responsible for quoting
/// each one for the remote shell.
pub fn build_remote_command(plan: &RemoteSpawnPlan) -> Vec<String> {
    let mut argv = vec![
        "env".to_owned(),
        format!("BOSS_RUN_ID={}", plan.run_id),
        format!("BOSS_EVENTS_SOCKET={}", plan.events_socket_path),
        format!("BOSS_LEASE_ID={}", plan.lease_id),
        format!("BOSS_WORKSPACE={}", plan.workspace_path),
    ];
    if let Some(url) = &plan.repo_remote_url {
        argv.push(format!("BOSS_REPO_REMOTE_URL={url}"));
    }
    if let Some(file) = &plan.initial_input_file {
        argv.push(format!("BOSS_INITIAL_INPUT_FILE={file}"));
    }
    if let Some(file) = &plan.settings_file {
        argv.push(format!("BOSS_SETTINGS_FILE={file}"));
    }
    argv.push(plan.wrapper_path.clone());
    argv
}

/// Parse `pid=<n>` out of the wrapper's `boss-remote-run: starting …`
/// stderr line so the engine can persist `work_runs.remote_pid`.
pub fn parse_remote_pid(stderr: &str) -> Option<i64> {
    for line in stderr.lines() {
        if let Some(idx) = line.find("pid=") {
            let digits: String = line[idx + 4..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

// ── Transport seam ────────────────────────────────────────────────────────────

/// The narrow slice of transport operations the remote launch needs.
/// Implemented by [`SshTransport`] (real) and by a fake in tests so the
/// orchestration runs in-process.
#[async_trait]
pub trait SshExec: Send + Sync {
    /// Stable host id (for logging / outcome detail).
    fn host_id(&self) -> &str;
    /// Run a command on the remote over the master, capturing output.
    async fn run(&self, argv: &[&str]) -> Result<SshOutput>;
    /// `ssh -O forward -R remote:local` over the master.
    async fn add_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput>;
    /// `ssh -O cancel -R remote:local` over the master.
    async fn cancel_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput>;
}

#[async_trait]
impl SshExec for SshTransport {
    fn host_id(&self) -> &str {
        &self.host_id
    }
    async fn run(&self, argv: &[&str]) -> Result<SshOutput> {
        SshTransport::run(self, argv).await
    }
    async fn add_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput> {
        SshTransport::add_reverse_unix_forward(self, remote_socket, local_socket).await
    }
    async fn cancel_reverse_unix_forward(
        &self,
        remote_socket: &str,
        local_socket: &str,
    ) -> Result<SshOutput> {
        SshTransport::cancel_reverse_unix_forward(self, remote_socket, local_socket).await
    }
}

// ── Orchestration ─────────────────────────────────────────────────────────────

/// Result of [`perform_remote_launch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLaunchOutcome {
    /// True iff the wrapper backgrounded the worker (exit 0).
    pub launched: bool,
    /// Failure-reason taxonomy string when `launched` is false.
    pub failure_reason: Option<&'static str>,
    /// Worker pid on the remote, parsed from the wrapper handshake.
    pub remote_pid: Option<i64>,
    /// Human-readable detail (stderr / exit) for diagnostics.
    pub detail: Option<String>,
}

/// Open the events tunnel and launch the detached remote worker.
///
/// Sequence (all over the one master multiplex):
/// 1. `rm -f` any stale remote events socket so `-O forward` can bind
///    (handles the "no stale socket left behind" requirement on the
///    remote side, and lets engine restart re-establish cleanly).
/// 2. `ssh -O forward -R <remote sock>:<engine sock>` — the events tunnel.
/// 3. Run the wrapper, which validates its contract and backgrounds the
///    worker (`nohup`), returning quickly.
/// 4. On wrapper failure, cancel the forward we just opened so it does
///    not leak, and map the sentinel exit to a failure reason.
///
/// The worker is detached (the wrapper's `nohup` + the engine not
/// holding the launch ssh open), so it survives the engine restarting;
/// its completion is driven later by the `Stop` hook over the forwarded
/// socket — not by this function blocking on the worker.
pub async fn perform_remote_launch(
    exec: &dyn SshExec,
    plan: &RemoteSpawnPlan,
    engine_events_socket: &str,
) -> Result<RemoteLaunchOutcome> {
    // 1. Clear any stale remote socket from a previous run / crash.
    let _ = exec
        .run(&["rm", "-f", plan.events_socket_path.as_str()])
        .await?;

    // 2. Establish the events forward over the master.
    let fwd = exec
        .add_reverse_unix_forward(&plan.events_socket_path, engine_events_socket)
        .await?;
    if !fwd.success() {
        return Ok(RemoteLaunchOutcome {
            launched: false,
            failure_reason: Some(REASON_WORKER_LAUNCH_FAILED),
            remote_pid: None,
            detail: Some(non_empty_detail(&fwd, "events forward failed")),
        });
    }

    // 3. Launch the (detached) worker.
    let argv = build_remote_command(plan);
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = exec.run(&argv_refs).await?;

    match classify_wrapper_exit(out.status) {
        WrapperLaunch::Launched => Ok(RemoteLaunchOutcome {
            launched: true,
            failure_reason: None,
            remote_pid: parse_remote_pid(&out.stderr),
            detail: None,
        }),
        WrapperLaunch::Failed(reason) => {
            // 4. Don't leak the forward when the launch itself failed.
            let _ = exec
                .cancel_reverse_unix_forward(&plan.events_socket_path, engine_events_socket)
                .await;
            Ok(RemoteLaunchOutcome {
                launched: false,
                failure_reason: Some(reason),
                remote_pid: None,
                detail: Some(non_empty_detail(&out, "wrapper exited non-zero")),
            })
        }
    }
}

/// Prefer the command's stderr (trimmed) for the failure detail; fall
/// back to a synthetic `exit N` string so the detail is never empty.
fn non_empty_detail(out: &SshOutput, fallback: &str) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        stderr.to_owned()
    } else {
        format!("{fallback} (exit {})", out.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── Pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn remote_events_socket_path_is_run_scoped() {
        assert_eq!(
            remote_events_socket_path("exec_18b4_8a"),
            "/tmp/boss-events-exec_18b4_8a.sock",
        );
        // Every char outside [A-Za-z0-9_-] is mapped to `_`, so a `..`
        // path-traversal attempt is neutralised (both the slashes AND the
        // dots collapse to underscores) and the socket path is unambiguous.
        assert_eq!(
            remote_events_socket_path("run/../etc"),
            "/tmp/boss-events-run____etc.sock",
        );
    }

    #[test]
    fn classify_wrapper_exit_maps_every_sentinel() {
        assert_eq!(classify_wrapper_exit(0), WrapperLaunch::Launched);
        assert_eq!(
            classify_wrapper_exit(78),
            WrapperLaunch::Failed(REASON_WRAPPER_MISCONFIGURED)
        );
        assert_eq!(
            classify_wrapper_exit(79),
            WrapperLaunch::Failed(REASON_MISSING_CLAUDE)
        );
        assert_eq!(
            classify_wrapper_exit(80),
            WrapperLaunch::Failed(REASON_MISSING_CUBE)
        );
        assert_eq!(
            classify_wrapper_exit(81),
            WrapperLaunch::Failed(REASON_MISSING_GH)
        );
        assert_eq!(
            classify_wrapper_exit(1),
            WrapperLaunch::Failed(REASON_WORKER_LAUNCH_FAILED)
        );
        assert_eq!(
            classify_wrapper_exit(137),
            WrapperLaunch::Failed(REASON_WORKER_LAUNCH_FAILED)
        );
    }

    #[test]
    fn build_remote_command_includes_required_env_and_wrapper_last() {
        let plan = RemoteSpawnPlan {
            run_id: "run-1".into(),
            lease_id: "lease-1".into(),
            workspace_path: "/ws/mono-agent-007".into(),
            repo_remote_url: Some("git@example.com:me/mono.git".into()),
            events_socket_path: "/tmp/boss-events-run-1.sock".into(),
            initial_input_file: Some("/ws/mono-agent-007/.boss/initial-input.txt".into()),
            settings_file: Some("/ws/mono-agent-007/.boss/settings.json".into()),
            wrapper_path: "~/.boss-remote/bin/boss-remote-run".into(),
        };
        let argv = build_remote_command(&plan);
        assert_eq!(argv[0], "env");
        assert!(argv.contains(&"BOSS_RUN_ID=run-1".to_owned()));
        assert!(argv.contains(&"BOSS_EVENTS_SOCKET=/tmp/boss-events-run-1.sock".to_owned()));
        assert!(argv.contains(&"BOSS_LEASE_ID=lease-1".to_owned()));
        assert!(argv.contains(&"BOSS_WORKSPACE=/ws/mono-agent-007".to_owned()));
        assert!(argv.contains(&"BOSS_REPO_REMOTE_URL=git@example.com:me/mono.git".to_owned()));
        assert!(argv.contains(
            &"BOSS_INITIAL_INPUT_FILE=/ws/mono-agent-007/.boss/initial-input.txt".to_owned()
        ));
        assert!(argv.contains(
            &"BOSS_SETTINGS_FILE=/ws/mono-agent-007/.boss/settings.json".to_owned()
        ));
        // The wrapper path is always the final token.
        assert_eq!(argv.last().unwrap(), "~/.boss-remote/bin/boss-remote-run");
    }

    #[test]
    fn build_remote_command_omits_optional_env_when_absent() {
        let plan = RemoteSpawnPlan {
            run_id: "run-2".into(),
            lease_id: "lease-2".into(),
            workspace_path: "/ws".into(),
            repo_remote_url: None,
            events_socket_path: "/tmp/s.sock".into(),
            initial_input_file: None,
            settings_file: None,
            wrapper_path: "wrapper".into(),
        };
        let argv = build_remote_command(&plan);
        assert!(!argv.iter().any(|a| a.starts_with("BOSS_REPO_REMOTE_URL=")));
        assert!(!argv.iter().any(|a| a.starts_with("BOSS_INITIAL_INPUT_FILE=")));
        assert!(!argv.iter().any(|a| a.starts_with("BOSS_SETTINGS_FILE=")));
    }

    #[test]
    fn parse_remote_pid_reads_handshake_line() {
        let stderr = "boss-remote-run: starting run_id=run-1 version=eng-abc pid=43117\n";
        assert_eq!(parse_remote_pid(stderr), Some(43117));
        assert_eq!(parse_remote_pid("no pid here\n"), None);
        // Trailing non-digits after the number are ignored.
        assert_eq!(parse_remote_pid("x pid=12 done"), Some(12));
    }

    // ── Orchestration against a stubbed transport ────────────────────────────

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Run(Vec<String>),
        AddForward { remote: String, local: String },
        CancelForward { remote: String, local: String },
    }

    /// Stubbed transport: records every call and returns a canned exit
    /// status for the wrapper-launch `run` (the one whose argv[0] ==
    /// "env"). Forward add succeeds unless `fail_forward` is set.
    struct FakeExec {
        calls: Mutex<Vec<Call>>,
        wrapper_exit: i32,
        wrapper_stderr: String,
        fail_forward: bool,
    }

    impl FakeExec {
        fn new(wrapper_exit: i32, wrapper_stderr: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                wrapper_exit,
                wrapper_stderr: wrapper_stderr.to_owned(),
                fail_forward: false,
            }
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SshExec for FakeExec {
        fn host_id(&self) -> &str {
            "fake"
        }
        async fn run(&self, argv: &[&str]) -> Result<SshOutput> {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            self.calls.lock().unwrap().push(Call::Run(owned.clone()));
            // The wrapper launch is the `env …` invocation; the stale-
            // socket cleanup is `rm -f …` and always succeeds.
            if owned.first().map(String::as_str) == Some("env") {
                Ok(SshOutput {
                    status: self.wrapper_exit,
                    stdout: String::new(),
                    stderr: self.wrapper_stderr.clone(),
                })
            } else {
                Ok(SshOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        async fn add_reverse_unix_forward(
            &self,
            remote_socket: &str,
            local_socket: &str,
        ) -> Result<SshOutput> {
            self.calls.lock().unwrap().push(Call::AddForward {
                remote: remote_socket.to_owned(),
                local: local_socket.to_owned(),
            });
            Ok(SshOutput {
                status: if self.fail_forward { 255 } else { 0 },
                stdout: String::new(),
                stderr: if self.fail_forward {
                    "mux_client_forward: forwarding request failed".to_owned()
                } else {
                    String::new()
                },
            })
        }
        async fn cancel_reverse_unix_forward(
            &self,
            remote_socket: &str,
            local_socket: &str,
        ) -> Result<SshOutput> {
            self.calls.lock().unwrap().push(Call::CancelForward {
                remote: remote_socket.to_owned(),
                local: local_socket.to_owned(),
            });
            Ok(SshOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn sample_plan() -> RemoteSpawnPlan {
        RemoteSpawnPlan {
            run_id: "run-1".into(),
            lease_id: "lease-1".into(),
            workspace_path: "/ws".into(),
            repo_remote_url: None,
            events_socket_path: remote_events_socket_path("run-1"),
            initial_input_file: None,
            settings_file: None,
            wrapper_path: "~/.boss-remote/bin/boss-remote-run".into(),
        }
    }

    #[tokio::test]
    async fn happy_path_clears_socket_opens_forward_then_launches() {
        let exec = FakeExec::new(
            0,
            "boss-remote-run: starting run_id=run-1 version=eng-x pid=4242\n",
        );
        let engine_sock = "/Users/me/Library/Application Support/Boss/events.sock";
        let outcome = perform_remote_launch(&exec, &sample_plan(), engine_sock)
            .await
            .unwrap();

        assert!(outcome.launched);
        assert_eq!(outcome.failure_reason, None);
        assert_eq!(outcome.remote_pid, Some(4242));

        let calls = exec.calls();
        // Order matters: stale-socket cleanup → forward → wrapper launch.
        assert_eq!(
            calls[0],
            Call::Run(vec![
                "rm".into(),
                "-f".into(),
                "/tmp/boss-events-run-1.sock".into(),
            ])
        );
        assert_eq!(
            calls[1],
            Call::AddForward {
                remote: "/tmp/boss-events-run-1.sock".into(),
                local: engine_sock.into(),
            }
        );
        match &calls[2] {
            Call::Run(argv) => {
                assert_eq!(argv[0], "env");
                assert!(argv.contains(&"BOSS_RUN_ID=run-1".to_owned()));
                assert!(argv.contains(
                    &"BOSS_EVENTS_SOCKET=/tmp/boss-events-run-1.sock".to_owned()
                ));
            }
            other => panic!("expected wrapper launch run, got {other:?}"),
        }
        // No cancel on the happy path — the forward must persist for the run.
        assert!(!calls.iter().any(|c| matches!(c, Call::CancelForward { .. })));
    }

    #[tokio::test]
    async fn wrapper_sentinel_failure_maps_reason_and_cancels_forward() {
        // 79 == claude missing.
        let exec = FakeExec::new(79, "boss-remote-run: `claude` not found on PATH\n");
        let outcome = perform_remote_launch(&exec, &sample_plan(), "/engine.sock")
            .await
            .unwrap();

        assert!(!outcome.launched);
        assert_eq!(outcome.failure_reason, Some(REASON_MISSING_CLAUDE));
        assert!(outcome.detail.unwrap().contains("claude"));

        // The forward we opened before the failed launch is torn down.
        assert!(exec
            .calls()
            .iter()
            .any(|c| matches!(c, Call::CancelForward { .. })));
    }

    #[tokio::test]
    async fn forward_failure_short_circuits_before_launch() {
        let mut exec = FakeExec::new(0, "");
        exec.fail_forward = true;
        let outcome = perform_remote_launch(&exec, &sample_plan(), "/engine.sock")
            .await
            .unwrap();

        assert!(!outcome.launched);
        assert_eq!(outcome.failure_reason, Some(REASON_WORKER_LAUNCH_FAILED));
        // The wrapper launch must never run if the forward didn't come up.
        assert!(!exec.calls().iter().any(|c| matches!(
            c,
            Call::Run(argv) if argv.first().map(String::as_str) == Some("env")
        )));
    }
}
