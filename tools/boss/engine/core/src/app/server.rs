//! Server startup, event-accept loops, and process-management helpers.
//!
//! Split out of `app.rs`; all startup / shutdown infrastructure lives here.
//! Pure structural move — no behavioural change.

use super::*;

pub async fn run(cli: Cli) -> Result<()> {
    let socket_str = cli.socket_path.as_deref().unwrap_or(DEFAULT_SOCKET_PATH);
    let isolation = IsolationPaths::derive(socket_str);

    // Build WorkConfig, overriding db_path when the isolation guard derived one.
    // This must happen before RuntimeConfig so the DB the engine opens is
    // already the isolated one — not the production state.db that
    // WorkConfig::load_from_env() would resolve from $HOME.
    let mut work = crate::config::WorkConfig::load_from_env()?;
    if let Some(ref iso_db) = isolation.db_path {
        work.db_path = iso_db.clone();
    }
    let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(work, None));

    if isolation.is_test_fixture {
        tracing::info!(
            cwd = %cfg.work.cwd.display(),
            db_path = %cfg.work.db_path.display(),
            events_socket = ?isolation.events_socket,
            pid_path = ?isolation.pid_path,
            "test-fixture mode: isolated paths derived from non-default socket; \
             production state (events.sock, state.db, pid file) will not be touched"
        );
    } else {
        tracing::info!(
            cwd = %cfg.work.cwd.display(),
            db_path = %cfg.work.db_path.display(),
            "starting boss-engine runtime",
        );
    }

    run_server(cli, cfg, isolation).await
}

async fn run_server(cli: Cli, cfg: Arc<RuntimeConfig>, isolation: IsolationPaths) -> Result<()> {
    let socket_path = cli
        .socket_path
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());

    // Use the isolation-derived pid path, falling back to env / hard default.
    let pid_file_path = isolation
        .pid_path
        .or_else(|| {
            std::env::var("BOSS_ENGINE_PID_PATH")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| DEFAULT_PID_PATH.into());

    // Use the isolation-derived events socket, falling back to env / home default.
    let events_socket_path = isolation
        .events_socket
        .map(Ok)
        .unwrap_or_else(default_events_socket_path)?;

    let control_token_path = crate::engine_control::default_token_path();

    // Orphan watcher: when the engine is a test fixture (non-default socket),
    // watch the parent process pid.  If the parent exits (e.g. a `bazel test`
    // runner that failed mid-run), this engine should exit too rather than
    // becoming an orphan that keeps production state bound.
    let watched_parent_pid = if isolation.is_test_fixture {
        let ppid = unsafe { libc::getppid() };
        tracing::debug!(parent_pid = ppid, "orphan watcher armed");
        Some(ppid)
    } else {
        None
    };

    serve(
        cfg,
        socket_path.into(),
        Some(pid_file_path),
        Some(events_socket_path),
        control_token_path,
        watched_parent_pid,
    )
    .await
}

fn default_events_socket_path() -> Result<std::path::PathBuf> {
    if let Ok(override_path) = std::env::var("BOSS_EVENTS_SOCKET") {
        return Ok(override_path.into());
    }
    let Some(home) = std::env::var_os("HOME") else {
        bail!("HOME must be set to derive the default events socket path");
    };
    Ok(std::path::PathBuf::from(home).join("Library/Application Support/Boss/events.sock"))
}

/// Return `true` if the process at `pid` is still alive on this machine.
///
/// Uses `kill(pid, 0)` (signal 0 = probe, no signal delivered): returns `true`
/// when the kernel confirms the process exists.  `EPERM` (process exists but
/// we can't signal it) also counts as alive; only `ESRCH` (no such process)
/// means dead.
pub fn process_is_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno == libc::EPERM
}

/// Spawn the GitHub OAuth auth-state forwarder.
///
/// At boot it restores any keychain-persisted token (so the status surface
/// reflects a prior connection across engine restarts), then subscribes to the
/// controller's state channel and, for every transition:
/// - pushes the display-safe [`GitHubAuthStateDto`] on the `github.auth` topic
///   so subscribed frontends re-render, and
/// - when the state is freshly `Authorized` with an unresolved `org_state`,
///   runs the org/SSO probe ([`probe_and_record_org_state`]) and records the
///   result via `update_org_state` — which itself produces the next transition
///   the loop then broadcasts.
///
/// The probe only fires while `org_state` is `Unknown`, so resolving it to
/// `Ok`/`NeedsOrgApproval`/`NeedsSso` does not re-trigger a probe; a probe that
/// returns `Unknown` (transient / no org binding) leaves the state unchanged,
/// so the loop simply waits for the next real transition rather than spinning.
fn spawn_github_auth_forwarder(server_state: Arc<ServerState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let controller = server_state.github_auth.clone();
        // Restore a persisted token (if any) before subscribing, so the first
        // loop iteration sees the restored `Authorized { Unknown }` state and
        // runs the org probe.
        controller.restore_from_store();

        let flow = controller.device_flow();
        let work_db = server_state.work_db.clone();
        let mut rx = controller.subscribe();
        loop {
            let state = rx.borrow_and_update().clone();
            server_state
                .broadcast_github_auth_state(state.to_dto())
                .await;

            if let GitHubAuthState::Authorized {
                record,
                org_state: OrgAuthState::Unknown,
            } = &state
            {
                let token = record.token.clone();
                let resolved =
                    probe_and_record_org_state(work_db.as_ref(), flow.as_ref(), &token).await;
                controller.update_org_state(resolved);
            }

            if rx.changed().await.is_err() {
                // Sender dropped — the engine is shutting down.
                break;
            }
        }
    })
}

/// Run the frontend server until the listener fails.
///
/// `socket_path` is bound exclusively (the file is removed first if it exists).
/// When `pid_file_path` is `Some`, the engine writes its pid there and removes
/// the file on shutdown — pass `None` from in-process tests to avoid touching
/// shared filesystem state. When `events_socket_path` is `Some`, the engine
/// also binds the worker events socket (mode 0600) and runs an accept loop
/// that decodes hook payloads via the worker registry; pass `None` from
/// tests that don't exercise the events channel.
///
/// When `control_token_path` is `Some`, the engine mints a random
/// secret on startup, writes it to that path (mode 0600), and accepts
/// matching `Shutdown { token }` RPCs on the frontend socket. The
/// file is removed on graceful exit via [`crate::engine_control::ControlTokenGuard`].
/// Tests pass `None` to skip the file entirely; in-process callers
/// own the runtime handle and don't need an authenticated wire path.
///
/// When `watched_parent_pid` is `Some(ppid)`, a background task polls
/// `kill(ppid, 0)` once per second; if the process is gone the task fires an
/// orphan-shutdown trigger that causes this function to return `Ok(())`.
/// Pass `None` from in-process tests that don't need orphan detection.
pub async fn serve(
    cfg: Arc<RuntimeConfig>,
    socket_path: std::path::PathBuf,
    pid_file_path: Option<std::path::PathBuf>,
    events_socket_path: Option<std::path::PathBuf>,
    control_token_path: Option<std::path::PathBuf>,
    watched_parent_pid: Option<libc::pid_t>,
) -> Result<()> {
    let app_pid = current_parent_pid();
    let (control_token, _control_token_guard) = match control_token_path {
        Some(path) => {
            let token = crate::engine_control::generate_token();
            let contents = crate::engine_control::ControlTokenFile {
                token: token.clone(),
                socket_path: socket_path.display().to_string(),
                pid: std::process::id(),
            };
            crate::engine_control::write_token_file(&path, &contents).with_context(|| {
                format!(
                    "failed to write engine-control token file {}",
                    path.display()
                )
            })?;
            tracing::info!(
                token_path = %path.display(),
                "engine-control token: ready",
            );
            let guard =
                crate::engine_control::ControlTokenGuard::new(path.clone(), std::process::id());
            (Some(Arc::new(token)), Some(guard))
        }
        None => (None, None),
    };
    let server_state =
        ServerState::new_arc_with_app_pid(cfg.clone(), app_pid, control_token.clone())?;

    // Always attempt to unlink any existing file at the path before
    // binding. `path.exists()` lies for dangling symlinks and races
    // with concurrent file ops; just call `remove_file` and ignore
    // `NotFound`. A stale file from a previous engine that crashed
    // without cleanup is the exact failure shape the 2026-05-07
    // incident left behind on `events.sock`; mirror the defensive
    // unlink here so the frontend socket can't develop the same drift.
    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {
            tracing::info!(
                socket_path = %socket_path.display(),
                "frontend socket: unlinked stale file before bind",
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "failed to remove existing socket {}",
                socket_path.display()
            )));
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => {
            crate::audit::record_socket_bind(
                "frontend",
                &socket_path,
                crate::audit::SocketBindResult::Succeeded,
            );
            listener
        }
        Err(err) => {
            let msg = err.to_string();
            crate::audit::record_socket_bind(
                "frontend",
                &socket_path,
                crate::audit::SocketBindResult::Failed(&msg),
            );
            return Err(anyhow::Error::new(err).context(format!(
                "failed to bind unix socket {}",
                socket_path.display()
            )));
        }
    };

    let _pid_guard = match pid_file_path {
        Some(path) => {
            let path_str = path.to_string_lossy().into_owned();
            let pid = std::process::id();
            std::fs::write(&path, format!("{pid}\n"))
                .with_context(|| format!("failed to write pid file {path_str}"))?;
            tracing::info!(pid, pid_file = %path_str, "engine pid file is ready");
            Some(PidFileGuard {
                path: path_str,
                pid,
            })
        }
        None => None,
    };

    tracing::info!(socket_path = %socket_path.display(), "frontend socket is ready");
    println!("boss-engine listening on {}", socket_path.display());

    if let Some(path) = events_socket_path {
        let events_listener = match bind_events_socket(&path) {
            Ok(listener) => {
                crate::audit::record_socket_bind(
                    "events",
                    &path,
                    crate::audit::SocketBindResult::Succeeded,
                );
                listener
            }
            Err(err) => {
                let msg = err.to_string();
                crate::audit::record_socket_bind(
                    "events",
                    &path,
                    crate::audit::SocketBindResult::Failed(&msg),
                );
                return Err(anyhow::Error::new(err)
                    .context(format!("failed to bind events socket {}", path.display())));
            }
        };
        tracing::info!(events_socket_path = %path.display(), "events socket is ready");
        let server_state_for_events = server_state.clone();
        tokio::spawn(async move {
            run_events_accept_loop(events_listener, server_state_for_events).await;
        });
    }

    // First, sweep "ghost active" rows that the previous engine left
    // behind without ever spawning a worker — `tasks.status = 'active'`
    // with no `work_runs` history at all. These are demoted back to
    // `todo` so `boss chore list --status active` and
    // `bossctl agents list` can't drift apart on the strength of a
    // chore that never reached a slot. Items with run history are
    // left alone for `reconcile_active_dispatch` below to redispatch.
    match server_state.work_db.heal_ghost_active_chores() {
        Ok(healed) if !healed.is_empty() => {
            let ids: Vec<&str> = healed.iter().map(|h| h.work_item_id.as_str()).collect();
            tracing::warn!(
                count = healed.len(),
                ids = ?ids,
                "demoted ghost-active chores with no run history",
            );
            // Publish an invalidation on each owning product topic so
            // subscribed kanban views refetch and move the card out of
            // Doing immediately — without this the engine's demotion
            // stays invisible to the UI until the next manual refresh,
            // which is the silent-divergence half of #680.
            for h in &healed {
                server_state
                    .publisher
                    .publish_work_item_changed(
                        &h.product_id,
                        &h.work_item_id,
                        "ghost-active demotion: dispatch never reached a worker",
                    )
                    .await;
            }
        }
        Ok(_) => {
            tracing::debug!("no ghost-active chores to demote at startup");
        }
        Err(err) => {
            tracing::error!(?err, "ghost-active sweep failed; continuing");
        }
    }

    // Install boss-event to a stable location and heal existing worker
    // settings.json files. This ensures that hook paths baked into worker
    // settings.json survive a `bazel clean` or workspace re-lease.
    //
    // Resolution at install time intentionally skips the stable-bin-dir
    // candidate (pass None) so we always copy the real binary from its
    // original source rather than potentially re-copying a previous install.
    let stable_boss_event_path = {
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace_dir = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        let current_shim = crate::runner::resolve_boss_event_binary(
            &engine_path,
            workspace_dir.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            None,
        );
        if let Some(home) = std::env::var_os("HOME") {
            let stable_bin_dir = PathBuf::from(home).join("Library/Application Support/Boss/bin");
            match crate::runner::install_boss_event_to_stable_bin(&current_shim, &stable_bin_dir) {
                Ok(stable) => {
                    tracing::info!(
                        stable_path = %stable.display(),
                        source_path = %current_shim.display(),
                        "boss-event installed to stable bin dir",
                    );
                    stable
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        source_path = %current_shim.display(),
                        "failed to install boss-event to stable bin dir; \
                         new workers will use the resolved path",
                    );
                    current_shim
                }
            }
        } else {
            current_shim
        }
    };

    // Heal existing worker settings files so a worker whose baked hook
    // path went stale (e.g. after a `bazel clean`) picks up the stable
    // boss-event path on the next engine restart. The settings files
    // live under the system temp dir, outside every workspace — see
    // `worker_setup` module docs.
    let worker_settings_dir = crate::worker_setup::worker_settings_dir();
    tracing::info!(
        dir = %worker_settings_dir.display(),
        new_path = %stable_boss_event_path.display(),
        "healing boss-event path in worker settings files",
    );
    crate::worker_setup::heal_worker_settings_json(&worker_settings_dir, &stable_boss_event_path);

    // Rehydrate dispatch for any work items that were in "Doing"
    // (status=active) when the engine last shut down but whose
    // executions ended without being moved out of the column. See
    // `tools/boss/docs/designs/work-kanban.md` §3 — the Doing column
    // is supposed to mirror "running or queued," and on startup we
    // re-issue RequestExecution for items that no longer satisfy
    // either half of that contract.
    //
    // On startup the in-memory live-worker registry is empty, so we
    // can't use it as the "is the worker still attached" oracle —
    // taking it at face value would treat every persisted in-flight
    // execution as orphaned and spawn a *second* worker on top of the
    // one already running. That's the duplicate-dispatch bug observed
    // on 2026-05-07 (slot 1+7 / slot 4+8 each on the same chore).
    //
    // Instead, probe `cube workspace list` once and mark every
    // persisted in-flight execution Live / Dead / Unknown based on
    // whether its lease is still bound to the same workspace. The
    // events socket is intentionally NOT consulted (it can be the
    // first thing to break on a crash). See `crate::run_reconcile`
    // for the verdict rules.
    let in_flight = match server_state.work_db.list_in_flight_executions() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to list in-flight executions for startup reconcile; continuing without per-run probe (existing reconcile path may double-dispatch)"
            );
            Vec::new()
        }
    };
    let probe_report = if in_flight.is_empty() {
        tracing::debug!("no persisted in-flight executions to probe at startup");
        crate::run_reconcile::RunReconcileReport::default()
    } else {
        let now_epoch_s = crate::run_reconcile::current_epoch_s();
        let report = crate::run_reconcile::probe_in_flight_runs(
            server_state.cube_client.as_ref(),
            &in_flight,
            now_epoch_s,
        )
        .await;
        tracing::info!(
            in_flight_count = in_flight.len(),
            live = report.live_count,
            dead = report.dead_count,
            unknown = report.unknown_count,
            "engine startup: probed persisted in-flight runs against cube state",
        );
        if report.unknown_count > 0 {
            tracing::warn!(
                unknown = report.unknown_count,
                "startup reconcile produced Unknown verdicts; those work items will NOT be auto-redispatched — operator should investigate"
            );
        }
        report
    };
    let skip_dispatch_ids: HashSet<String> = probe_report
        .skip_dispatch_ids()
        .map(|s| s.to_owned())
        .collect();

    // Reap orphans before reconcile dispatch fires. For every Dead
    // verdict the cube probe returned, mark the execution row
    // `orphaned` (terminal) so the subsequent `reconcile_active_dispatch`
    // sees it as a finished predecessor and inherits its
    // `cube_workspace_id` into the new ready row's
    // `preferred_workspace_id`. The orphan reap intentionally does NOT
    // release the cube workspace lease — the workspace may still hold
    // in-flight commits the next worker should resume against.
    //
    // See docs/post-crash-recovery.md for the full flow.
    let orphan_reason = "engine startup: cube probe verdict Dead — worker lease no longer matches recorded state across restart";
    for (execution_id, verdict) in &probe_report.verdicts {
        if !matches!(verdict, crate::run_reconcile::RunReconcileVerdict::Dead) {
            continue;
        }
        match server_state
            .work_db
            .mark_execution_orphaned(execution_id, orphan_reason)
        {
            Ok(execution) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "startup reaper: marked execution orphaned (workspace preserved for re-lease)",
                );
                // Snapshot any uncommitted in-flight work to a durable
                // patch before the workspace can be re-leased/reset.
                // Best-effort and self-logging; never blocks the reaper.
                crate::recovery_backup::backup_dead_execution(&execution);
            }
            Err(err) => {
                // Already-terminal rows are benign here — a parallel
                // sweep (e.g. heal_ghost_active_chores) may have
                // closed the row first. Anything else is real and
                // worth surfacing.
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "startup reaper: skipped orphan reap (likely already terminal)",
                );
            }
        }
    }

    match server_state
        .work_db
        .reconcile_active_dispatch(|execution_id| skip_dispatch_ids.contains(execution_id))
    {
        Ok(redispatched) if !redispatched.is_empty() => {
            tracing::info!(
                count = redispatched.len(),
                ids = ?redispatched,
                "reconciled active-dispatch on startup",
            );
        }
        Ok(_) => {
            tracing::debug!("no active-dispatch reconcile needed at startup");
        }
        Err(err) => {
            tracing::error!(?err, "active-dispatch reconcile failed; continuing");
        }
    }

    // Spawn the database backup loop. Fires immediately on boot (startup
    // snapshot) and then every `backup_interval` (default: 1 hour).
    // Uses SQLite's VACUUM INTO for a crash-safe, WAL-compatible copy.
    // Interval and retention count are configurable via env vars; safe
    // defaults apply when they are not set. In-memory databases (tests)
    // are silently skipped.
    let db_backup_state_root: PathBuf = cfg
        .work
        .db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.work.cwd.clone());
    let _db_backup_handle = crate::database_backup::spawn_loop(
        server_state.work_db.clone(),
        crate::database_backup::default_backup_dir(&db_backup_state_root),
        crate::database_backup::backup_interval(),
        crate::database_backup::retention_count(),
    );

    // Spawn the merge-detection poller. Workers can land their PRs
    // long after their Stop event has fired (and lease has been
    // released), so the on-Stop completion path can't catch every
    // merge. The poller fills that gap by periodically asking GitHub
    // about every chore that's currently in_review with a pr_url and
    // promoting the merged ones to `done`. Polling cadence is
    // deliberately conservative — chores rarely sit in review for
    // long, and we don't want to spam `gh` from the engine process.
    let merge_probe: Arc<dyn MergeProbe> = Arc::new(CommandMergeProbe::new());
    let _merge_handle = spawn_merge_poller(
        server_state.work_db.clone(),
        merge_probe,
        server_state.publisher.clone(),
        server_state.cube_client.clone(),
        server_state.completion_handler.clone(),
        Duration::from_secs(60),
        server_state.metrics.clone(),
        server_state.pr_reconciler_kick.clone(),
    );

    // Periodic dead-PID reconciler: detects worker slots whose backing
    // OS process has died (kill-9, crash, OOM) and reaps them so the
    // orphan sweep can redispatch the chore. Runs every 60s and fires
    // immediately on boot. Without this, a kill-9'd worker leaves the
    // pool slot claimed forever and the orphan sweep skips the chore
    // ("already claimed"), leaving it stuck in Doing indefinitely.
    let _dead_pid_sweep_handle = crate::dead_pid_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Duration::from_secs(60),
    );

    // Periodic pool-claim reconciler: detects worker-pool slots still
    // claimed by an execution that is terminal in the DB and has NO live
    // worker pane, and releases the leaked claim. Every other release
    // path (completion's `release_worker_pane`, the dead-pid / stale-
    // worker / transient-recovery sweeps) keys off a *live* worker, so a
    // claim whose execution terminated without a live pane — a mid-spawn
    // cancel, a `finalize_pr_transition` DB-error early-return, a teardown
    // that dropped the run→slot mapping but not the claim, a
    // `bossctl agents stop` that freed the cube lease but not the claim —
    // is released by nothing and outlives its execution forever. Once all
    // automation slots leak this way, automation dispatch wedges with no
    // self-healing. This sweep walks the pool's OWN claimed slots (not the
    // live-state registry) to close that gap. Runs every 60s and fires on
    // boot so a pool wedged before a restart self-heals without an
    // operator restart.
    let _pool_claim_sweep_handle = crate::pool_claim_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        crate::pool_claim_sweep::DEFAULT_INTERVAL,
    );

    // Periodic stale-worker liveness backstop: detects worker slots whose
    // `claude` process is still alive but has made no transcript progress
    // (no hook event) for longer than the staleness threshold while
    // `activity=working` with no tool in flight. This is the wedged-
    // dependency hang from issue #976 — a worker that backgrounded its
    // pre-push bazel build and idled "until the gate is green" forever
    // when bazel never completed. The dead-PID sweep cannot catch it
    // (the process is alive), so this reaps the execution and releases
    // the slot for redispatch. Runs every 60s and fires on boot.
    let _stale_worker_sweep_handle = crate::stale_worker_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Duration::from_secs(60),
        crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS,
    );

    // Periodic syspolicyd CPU monitor: detects when macOS's `syspolicyd`
    // daemon wedges in a ~100% CPU spin. While it is stuck it stops
    // servicing code-signing assessments, so every `dlopen` of a
    // signature-checked dylib blocks and ALL Bazel servers hang at JVM
    // startup — a silent, machine-wide build outage that looks like
    // "Bazel is broken" (issue #965). The monitor flips a shared flag
    // once saturation is sustained; `build_engine_health_report` reads it
    // to raise a banner naming the cause and the `sudo kill -9 <pid>`
    // remedy. Detection only — the engine cannot safely kill the daemon
    // unattended (SIP blocks `launchctl kickstart`).
    let _syspolicyd_monitor_handle = crate::syspolicyd_monitor::spawn_loop(
        server_state.syspolicyd_health.clone(),
        crate::syspolicyd_monitor::DEFAULT_SAMPLE_INTERVAL,
    );

    // Periodic transient-recovery reconciler: detects workers wedged by
    // a transient Claude API error (the interactive `claude` session
    // printed the error, ended its turn, and sits Idle while the chore
    // is unfinished) and auto-resumes them on the same workspace with
    // bounded retries + backoff, escalating non-retryable / cap-reached
    // failures for human attention. Runs every 60s and fires on boot.
    let _transient_recovery_handle = crate::transient_recovery::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Arc::clone(&server_state) as Arc<dyn crate::transient_recovery::WorkerNudger>,
        crate::transient_recovery::DEFAULT_INTERVAL,
    );

    // Periodic orphan-active reconciler: re-dispatches `active` work
    // items that have no live execution (the post-crash "stuck-in-Doing"
    // fix). Runs every 60s and fires immediately on boot so items left
    // orphaned by the previous crash are recovered without waiting for
    // the first interval.
    let _orphan_sweep_handle = crate::orphan_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Duration::from_secs(60),
    );

    // External-tracker reconciler: periodically pulls upstream issue state
    // into Boss's work-item taxonomy. Default cadence: 120 s (2 min) per
    // the design doc's §"Cadence" rationale (Design Q5). Fires immediately
    // on spawn so any drift accumulated while the engine was offline is
    // reconciled at boot without waiting for the first interval.
    let _external_tracker_handle = crate::external_tracker::reconcile::spawn_loop(
        server_state.work_db.clone(),
        server_state.tracker_registry.clone(),
        Duration::from_secs(120),
        server_state.metrics.clone(),
        server_state.clone(),
        server_state.tracker_credential_resolver.clone(),
    );

    // GitHub OAuth auth-state forwarder: restores any persisted token at boot,
    // then watches the controller's state machine and (a) pushes every
    // transition on the `github.auth` topic and (b) runs the org/SSO probe on
    // each freshly-Authorized state. See the OAuth device-flow design §3/§7.
    let _github_auth_handle = spawn_github_auth_forwarder(server_state.clone());

    // Dependency-unblock safety-net sweeper: periodically re-evaluates
    // every dependency-blocked work item and unblocks any whose gating
    // prerequisites have all reached a satisfied status. The primary
    // unblock path is event-driven (cascade inside the prereq-done
    // transaction), but that path can silently skip a row if the item's
    // last_status_actor was reset between the auto-block and the prereq
    // landing, or if the engine was offline at transition time. The
    // sweeper recovers those cases within one interval (≤30 s).
    // See dep_unblock_sweep.rs for the full incident trace.
    let coord_for_dep_unblock = server_state.execution_coordinator.clone();
    let _dep_unblock_handle = crate::dep_unblock_sweep::spawn_loop(
        server_state.work_db.clone(),
        Duration::from_secs(crate::dep_unblock_sweep::DEP_UNBLOCK_SWEEP_INTERVAL_SECS),
        server_state.metrics.clone(),
        Arc::new(move || coord_for_dep_unblock.kick()),
    );

    // Automation scheduler (maintenance-tasks.md, Maint task 5): each tick,
    // for every enabled `schedule` automation that is due, compute its
    // cron/timezone occurrence, enforce the open-task gate, apply catch-up /
    // skip-if-imminent, and write the decision to `automation_runs`. Fires
    // immediately on boot so a daily occurrence elapsed while the engine was
    // down is caught up without waiting a full interval. With zero automations
    // configured the loop is inert.
    //
    // Maint task 6: the fire seam now dispatches a real `automation_triage`
    // work_execution via `EngineTriageDispatcher` (creates the execution row
    // bound to the automation and kicks the coordinator's drain). The existing
    // `dispatch_not_before` / `pre_start_failure_count` machinery retries a
    // transient pre-start failure transparently; the completion handler's
    // outcome detector finalises the run once the worker reaches a decision.
    let coord_for_automation_triage = server_state.execution_coordinator.clone();
    let automation_triage_dispatcher: Arc<dyn crate::automation_scheduler::TriageDispatcher> =
        Arc::new(crate::automation_triage::EngineTriageDispatcher::new(
            server_state.work_db.clone(),
            Arc::new(move || coord_for_automation_triage.kick()),
        ));
    let _automation_scheduler_handle = crate::automation_scheduler::spawn_loop(
        server_state.work_db.clone(),
        automation_triage_dispatcher,
        server_state.automation_scheduler_kick.clone(),
    );

    // Scheduler heartbeat: periodic `kick()` so a ready row stranded
    // by a dropped wakeup (the `status_transition` → `request_recorded`
    // stall class — see `exec_18af3ba5259d32a8_12`, 2026-05-13) is
    // picked up within one interval instead of waiting for the 90s
    // orphan-active reconciler. Logs a `warn!` when a stranded row is
    // observed so an operator notices the dropped wakeup on the first
    // occurrence rather than only inferring it from the redispatch
    // event. PR #429's reconciler remains the safety net for execution
    // rows whose worker has died — the heartbeat only re-kicks the
    // scheduler, it does not abandon or insert rows.
    let _scheduler_heartbeat_handle = server_state
        .execution_coordinator
        .spawn_scheduler_heartbeat(Duration::from_secs(15));

    // Watch in-flight dispatch timelines for stalled stages and emit
    // a `stage_stalled` event when one sits past the threshold
    // without progressing. Read-only against the per-execution
    // dispatch.jsonl mirrors; never modifies dispatcher behavior.
    //
    // Per-stage overrides: the early dispatch handoffs (worker
    // claim → cube repo ensure → cube workspace lease) should
    // never sit for more than ~30s in healthy operation, so flag
    // them faster than the 120s default. The 2026-05-12 cube-lease
    // hang spent 46s in `worker_claimed` with no event firing
    // because the global threshold hadn't elapsed; a 30s override
    // catches it on the first sweep after the wedge.
    let stage_thresholds = crate::dispatch_reader::StageThresholds::new(Duration::from_secs(120))
        .with_override("worker_claimed", Duration::from_secs(30))
        .with_override("cube_repo_ensured", Duration::from_secs(60))
        .with_override("cube_workspace_lease_attempted", Duration::from_secs(30));
    let _stage_stalled_handle = crate::dispatch_reader::spawn_stage_stalled_detector(
        server_state.dispatch_event_root.clone(),
        server_state.dispatch_events.clone(),
        stage_thresholds,
        Duration::from_secs(15),
    );

    // Periodic metrics flush — snapshots the in-memory counter /
    // gauge registry into `state.db` every 30s so monotonic totals
    // survive engine restarts (see
    // `tools/boss/docs/designs/engine-counter-metrics-framework.md`
    // §"Persistence: state.db table"). The graceful-shutdown branch
    // below runs one final flush before returning so the last 0–30s
    // window of increments isn't lost on a normal exit.
    let _metrics_flush_handle = crate::metrics::spawn_flush_task(
        server_state.metrics.clone(),
        server_state.work_db.clone(),
    );

    // Periodic stalled-spawn detector: transitions workers from `Spawning`
    // to `WaitingForInput` when they've been stuck without any hook event
    // for longer than STALLED_SPAWN_THRESHOLD_SECS. The initial directory-trust
    // prompt that Claude Code shows before `SessionStart` (for Opus /
    // `--permission-mode auto` workers) blocks the run with no Notification hook,
    // so the normal detection path never fires. This sweep detects the stall and
    // flips the activity so the kanban dot + WorkerWaitingIndicator signal the
    // operator that attention is needed. Runs every 10 seconds — the prompt
    // appears at session startup, so fast detection matters.
    {
        let live_worker_states = server_state.live_worker_states.clone();
        let server_clone = Arc::clone(&server_state);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let changed = live_worker_states.mark_stalled_spawns(
                    now,
                    crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS,
                );
                if !changed.is_empty() {
                    tracing::info!(
                        slots = ?changed,
                        "stalled-spawn sweep: transitioned slots from Spawning to WaitingForInput \
                         (no hook event since spawn — likely blocked on initial directory-trust prompt)",
                    );
                    server_clone.broadcast_live_worker_states().await;
                }
            }
        });
    }

    let coordinator = server_state.execution_coordinator.clone();
    coordinator.kick();

    install_panic_hook(&server_state);

    // Orphan watcher: poll the watched parent pid every second.  When it's
    // gone (the bazel test runner that spawned us exited), fire a notify so
    // the accept loop below can exit cleanly instead of becoming a
    // long-lived orphan that holds production sockets / DB / pid file.
    // Only armed for test-fixture engines (watched_parent_pid is Some).
    let orphan_trigger = Arc::new(Notify::new());
    if let Some(ppid) = watched_parent_pid {
        let trigger = orphan_trigger.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !process_is_alive(ppid) {
                    tracing::warn!(
                        parent_pid = ppid,
                        "parent process exited — test-fixture engine orphaned; exiting cleanly"
                    );
                    trigger.notify_one();
                    break;
                }
            }
        });
    }

    tracing::info!(
        socket_path = %socket_path.display(),
        "frontend socket: accept loop started",
    );
    crate::audit::record_accept_loop_started("frontend", &socket_path);

    let shutdown_trigger_for_loop = server_state.shutdown_trigger.clone();
    let orphan_trigger_for_loop = orphan_trigger.clone();
    loop {
        tokio::select! {
            biased;
            signal = graceful_shutdown_signal() => {
                tracing::info!(signal, "shutdown signal received; releasing worker panes");
                crate::audit::record_shutdown(format!("signal:{signal}"));
                server_state
                    .shutdown_workers(Duration::from_secs(5), Duration::from_secs(1))
                    .await;
                // One final metrics flush so the 0–30s window of
                // increments between the last periodic flush and the
                // shutdown signal isn't dropped on a clean exit.
                // Crash-loss is acceptable for monotonic counts; a
                // clean exit can afford to do better.
                if let Err(err) = crate::metrics::flush_all(
                    &server_state.metrics,
                    &server_state.work_db,
                ) {
                    tracing::warn!(?err, "metrics: final flush on shutdown failed");
                }
                tracing::info!("engine shutdown complete");
                return Ok(());
            }
            _ = shutdown_trigger_for_loop.notified() => {
                tracing::info!("shutdown rpc accepted; releasing worker panes");
                crate::audit::record_shutdown("rpc");
                server_state
                    .shutdown_workers(Duration::from_secs(5), Duration::from_secs(1))
                    .await;
                if let Err(err) = crate::metrics::flush_all(
                    &server_state.metrics,
                    &server_state.work_db,
                ) {
                    tracing::warn!(?err, "metrics: final flush on shutdown failed");
                }
                tracing::info!("engine shutdown complete");
                return Ok(());
            }
            _ = orphan_trigger_for_loop.notified() => {
                tracing::info!("orphan shutdown: test-fixture parent is gone; exiting");
                crate::audit::record_shutdown("orphan");
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept.context("socket accept failed")?;
                // Capture peer pid synchronously before any async yield so the
                // shim's quick-close (or any other peer that doesn't linger)
                // can't race us into ENOTCONN.
                let peer_pid_value = peer_pid(&stream).ok();
                let server_state = server_state.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        handle_frontend_connection(stream, server_state, peer_pid_value).await
                    {
                        tracing::error!(?err, "frontend connection failed");
                    }
                });
            }
        }
    }
}

/// Constant-time byte comparison. Used by the shutdown-RPC token
/// gate so a wrong-length or wrong-content token can't be inferred
/// from response timing — the same costs as the real comparison,
/// regardless of where the mismatch lands.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Walk up `pid`'s process tree (bounded depth) checking whether
/// any ancestor matches one of `trust_roots`. Used to implement
/// `LOCAL_PEERPID` subtree-match auth: a peer running inside a
/// trusted process tree is treated as that tree's tier.
pub(super) fn is_descendant_of_any(pid: libc::pid_t, trust_roots: &[libc::pid_t]) -> bool {
    use crate::worker_registry::parent_pid;
    const TRUST_WALK_DEPTH: usize = 16;
    let mut current = pid;
    for _ in 0..TRUST_WALK_DEPTH {
        if trust_roots.contains(&current) {
            return true;
        }
        match parent_pid(current) {
            Ok(Some(parent)) => current = parent,
            Ok(None) | Err(_) => return false,
        }
    }
    false
}

/// Whether `pid` names a live process. Implemented with `kill(pid, 0)`,
/// which delivers no signal but performs the existence + permission
/// check: `Ok` means the process exists, `EPERM` means it exists but is
/// owned by another user (still alive), and `ESRCH` means no such
/// process. Used by `RegisterAppSession` to decide whether a stale app
/// trust root can be superseded by a relaunched app — only when the old
/// app process is genuinely gone.
pub(super) fn pid_is_alive(pid: libc::pid_t) -> bool {
    // Reject pid <= 0: `kill(0, _)` targets the caller's process group
    // and `kill(-pid, _)` a process group, neither of which is the
    // single-process liveness probe we want — interpreting their result
    // as "alive" would be wrong.
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs no action beyond the
    // existence/permission probe; we only read `errno` on failure.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Decide whether a `RegisterAppSession` from `peer_pid` should be
/// trusted, given the currently-pinned app trust root `current_app_pid`
/// and the engine's own pid. Extracted from the connection handler so
/// the trust transitions (matching pid, engine-ancestor, dead-old-app
/// reattach) are unit-testable. See the call site for the rationale of
/// each branch.
pub(super) fn register_app_session_trust_ok(
    current_app_pid: Option<libc::pid_t>,
    peer_pid: Option<libc::pid_t>,
    engine_pid: libc::pid_t,
) -> bool {
    match (current_app_pid, peer_pid) {
        (None, _) => true, // tests / no-trust-root mode
        (Some(expected), Some(observed)) => {
            observed == expected
                || is_descendant_of_any(engine_pid, &[observed])
                || !pid_is_alive(expected)
        }
        (Some(_), None) => false,
    }
}

/// Resolve the `last_status_actor` string for an RPC-driven status change.
///
/// Returns `"boss"` when the caller's process ancestry matches the registered
/// Boss-coordinator session pid; `"human"` otherwise. Engine-internal writers
/// stamp `"engine"` directly in SQL and never call this function.
pub(super) fn resolve_status_actor(server_state: &ServerState, peer_pid: Option<libc::pid_t>) -> &'static str {
    let boss_pid = server_state.current_boss_pid();
    if let (Some(boss_pid), Some(peer_pid)) = (boss_pid, peer_pid) {
        if is_descendant_of_any(peer_pid, &[boss_pid]) {
            return boss_protocol::LAST_STATUS_ACTOR_BOSS;
        }
    }
    boss_protocol::LAST_STATUS_ACTOR_HUMAN
}

pub(super) fn current_parent_pid() -> Option<libc::pid_t> {
    // BOSS_APP_PID is the only signal we trust to identify the app
    // tier. The macOS app sets it to its own pid before spawning the
    // engine — necessary because `bazel run` daemonizes its server,
    // reparenting the engine away from the app's process tree, so
    // `getppid()` lands on `bazel` (or launchd) instead of the app.
    //
    // When BOSS_APP_PID is unset we leave app_pid as None rather than
    // guessing from `getppid()`. Falling back to the parent yields a
    // wrong-but-confident answer in every dev setup that launches the
    // engine independently of the app (`bazel run` from a terminal,
    // direct invocation of the binary, etc.) — the engine pins its
    // trust root to bazel/launchd and then rejects every legitimate
    // `RegisterAppSession` from the real app, which kills dispatch
    // (every `SpawnWorkerPane` request fails because no app session
    // is registered to receive it). With None, the trust gate becomes
    // a no-op (matches the test path), the app registers, and the
    // Boss session pid takes over as the real trust root once
    // `RegisterBossSession` lands. Production is unaffected: the app
    // always sets BOSS_APP_PID via `EngineProcessController`.
    std::env::var("BOSS_APP_PID")
        .ok()
        .and_then(|raw| raw.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 1)
}

/// Send `SIGTERM` to every pid in `pids`, sleep `grace`, then send
/// `SIGKILL` to anything still alive. Used as the shutdown fallback
/// when the app teardown path didn't release the worker shell — and
/// from the panic hook, where we must not touch the runtime. The
/// loop keeps going past `EPERM` / `ESRCH` because the worker may
/// already be dead (good) or owned by another uid (we can't help).
/// Engine-side backstop reap of a worker's OS process tree on pane
/// release. The macOS app's `releaseWorkerPane` (→ `WorkerProcessKiller`)
/// is the primary reaper, but it cannot act when no app session is
/// registered, when the app is unresponsive, or when a wedged surface
/// reports no foreground pid. In those cases `release_worker_pane` used
/// to free the engine slot and the cube lease while the worker's
/// `claude` process kept running — the leak in #975, where `bossctl
/// agents stop` cleared the slot but left the OS process alive.
///
/// Fires `SIGTERM` at the *process group* of `shell_pid` synchronously
/// (so a `claude` and anything it spawned — e.g. an MCP stdio child —
/// go too), then escalates to `SIGKILL` on a detached task after
/// `grace` if the lead pid is still alive. A non-positive pid is a
/// no-op so callers need not branch on "pid not reported yet".
///
/// Synchronous SIGTERM + detached SIGKILL (rather than a blocking
/// ladder) keeps the release path — and the `bossctl agents stop`
/// round-trip behind it — prompt: by the time it returns the worker
/// has at minimum been asked to exit. Mirrors the app-side
/// `WorkerProcessKiller` ladder and the `signal_shell_pids` shutdown
/// fallback.
pub(super) fn reap_worker_process_tree(shell_pid: i32, grace: Duration) {
    if shell_pid <= 0 {
        return;
    }
    let pid = shell_pid as libc::pid_t;
    let target = process_group_signal_target(pid);
    // SAFETY: `pid` was recorded by us at spawn; a failed kill is not
    // fatal (the process may already have exited).
    let rc = unsafe { libc::kill(target, libc::SIGTERM) };
    tracing::debug!(pid, target, rc, "reap_worker_process_tree: SIGTERM");
    tokio::spawn(async move {
        if grace > Duration::from_secs(0) {
            tokio::time::sleep(grace).await;
        }
        if matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Dead
        ) {
            tracing::debug!(pid, "reap_worker_process_tree: exited after SIGTERM");
            return;
        }
        // SAFETY: same as above.
        let rc = unsafe { libc::kill(target, libc::SIGKILL) };
        tracing::info!(
            pid,
            target,
            rc,
            "reap_worker_process_tree: process survived SIGTERM grace; escalated to SIGKILL",
        );
    });
}

/// Resolve the `kill(2)` target for `pid`: the negated process group id
/// when `getpgid` succeeds (so the whole group is signalled, reaching
/// descendants), falling back to the bare pid when `getpgid` reports
/// the process is already gone. Mirrors the app-side
/// `WorkerProcessKiller.signalTarget`.
pub(super) fn process_group_signal_target(pid: libc::pid_t) -> libc::pid_t {
    // SAFETY: `getpgid` only reads kernel state for `pid`.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 0 { -pgid } else { pid }
}

pub(super) fn signal_shell_pids(pids: &[libc::pid_t], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    for &pid in pids {
        // SAFETY: `kill` with a pid we recorded ourselves; failure is
        // logged but not fatal.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGTERM returned non-zero (likely already exited)",
            );
        }
    }
    if grace > Duration::from_secs(0) {
        std::thread::sleep(grace);
    }
    for &pid in pids {
        // SAFETY: same as above.
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGKILL returned non-zero",
            );
        }
    }
}

/// Snapshot of the (slot_id, shell_pid) pairs currently registered as
/// live workers, intended for the panic-hook path: pulls just enough
/// state to fire `SIGTERM`/`SIGKILL` without touching the runtime,
/// async, or Tokio internals (any of which could deadlock during
/// unwind).
fn snapshot_live_shell_pids(server_state: &ServerState) -> Vec<libc::pid_t> {
    server_state
        .live_worker_states
        .snapshot()
        .into_iter()
        .filter_map(|s| (s.shell_pid > 0).then_some(s.shell_pid as libc::pid_t))
        .collect()
}

/// Install a panic hook that emergency-kills every recorded worker
/// shell pid before delegating to the previously-installed hook. The
/// async `release_worker_pane` path is unsafe inside an unwinding
/// runtime — we settle for the synchronous SIGTERM/SIGKILL fallback
/// so the worker tree doesn't outlive the engine.
///
/// We hold a `Weak` so the hook never keeps `ServerState` alive past
/// a normal shutdown.
fn install_panic_hook(server_state: &Arc<ServerState>) {
    let weak = Arc::downgrade(server_state);
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(server) = weak.upgrade() {
            let pids = snapshot_live_shell_pids(&server);
            if !pids.is_empty() {
                tracing::error!(
                    count = pids.len(),
                    "engine panic: emergency-killing worker shells before unwind",
                );
                signal_shell_pids(&pids, Duration::from_millis(500));
            }
        }
        prior(info);
    }));
}

/// Future that resolves when a graceful-shutdown signal arrives
/// (`SIGINT` or `SIGTERM`). Resolves to a static label naming which
/// signal fired so the caller can log it.
async fn graceful_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to install SIGTERM handler; only SIGINT will trigger graceful shutdown"
            );
            tokio::signal::ctrl_c().await.ok();
            return "SIGINT";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

async fn run_events_accept_loop(listener: UnixListener, server_state: Arc<ServerState>) {
    let local_addr = listener.local_addr().ok();
    let path_display = local_addr
        .as_ref()
        .and_then(|a| a.as_pathname())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_owned());
    tracing::info!(
        events_socket_path = %path_display,
        "events socket: accept loop started",
    );
    if let Some(p) = local_addr.as_ref().and_then(|a| a.as_pathname()) {
        crate::audit::record_accept_loop_started("events", p);
    }
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let server_state = server_state.clone();
                tokio::spawn(async move {
                    match handle_connection(stream).await {
                        Ok(incoming) => {
                            tracing::info!(
                                peer_pid = ?incoming.peer_pid,
                                run_id = ?incoming.run_id,
                                event = ?incoming.event,
                                "events socket: hook event received",
                            );
                            // Audit *before* the live-state fan-out
                            // so an engine-side mismatch in the
                            // dispatch path can't drop the audit line
                            // — the deny is enforced harness-side by
                            // claude already, this is the independent
                            // forensic record. See
                            // [`worker_sandbox_audit`] for why.
                            crate::worker_sandbox_audit::record_if_sandbox_attempt(
                                &server_state.dispatch_event_root,
                                incoming.run_id.as_deref(),
                                &incoming.event,
                            );
                            dispatch_live_worker_state(&server_state, &incoming).await;
                            // Editorial PreToolUse audit: evaluate every
                            // `gh pr|issue` Bash invocation against the
                            // product's editorial rules and record the
                            // decision in `editorial_actions`. Fire-and-
                            // forget; never blocks the event dispatch.
                            dispatch_editorial_on_pretooluse(&server_state, &incoming).await;
                            // Urgent probes fire on PostToolUse so
                            // the coordinator can redirect a worker
                            // mid-task without waiting for Stop. The
                            // tool call has already returned at this
                            // point, so no in-flight work is lost.
                            dispatch_urgent_probe_on_post_tool_use(&server_state, &incoming).await;
                            // ProbeReplied runs first: emit the reply for the
                            // prior probe before dispatching the next one so
                            // a single Stop never fires both reply and dispatch
                            // for the same probe (the reply text hasn't been
                            // written yet at dispatch time).
                            //
                            // Completion runs before probe dispatch: probes
                            // queued by the completion handler (e.g.
                            // PROBE_NO_PR) must be visible to `dispatch_probe_on_stop`
                            // so they are delivered on the *same* Stop that
                            // triggered them rather than stalling until the
                            // next Stop (which never comes for an idle worker).
                            dispatch_probe_reply_on_stop(&server_state, &incoming).await;
                            dispatch_completion_on_stop(&server_state, &incoming).await;
                            dispatch_probe_on_stop(&server_state, &incoming).await;
                        }
                        Err(err) => {
                            tracing::warn!(?err, "events socket: failed to handle connection");
                        }
                    }
                });
            }
            Err(err) => {
                tracing::error!(?err, "events socket accept failed");
            }
        }
    }
}
