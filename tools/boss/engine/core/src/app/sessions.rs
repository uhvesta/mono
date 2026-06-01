//! `FrontendRequest` handlers — app/boss session registration, engine responses, shutdown.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_register_app_session(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RegisterAppSession = req else {
        unreachable!()
    };
    {
        // Trust the peer if any of:
        //   (a) it matches the declared app pid exactly. The
        //       engine reads `BOSS_APP_PID` at startup; the
        //       macOS app sets this before spawning the engine
        //       (necessary because `bazel run` daemonizes,
        //       which severs the engine's process tree from
        //       the app and breaks ancestor-walk auth).
        //   (b) the peer pid appears in the engine's ancestor
        //       chain (covers direct-launch scenarios like
        //       `swift run` where no daemonizing wrapper
        //       exists).
        //   (c) APP RESTART against a surviving engine: the
        //       trusted app pid belongs to a now-dead process
        //       and a fresh app instance is connecting. The
        //       engine correctly stays up on a same-version
        //       relaunch, so the relaunched app must be able to
        //       re-attach its session — otherwise the stale pid
        //       rejects `RegisterAppSession` forever, no
        //       `app_session` is registered, and every
        //       engine→app RPC (`SpawnWorkerPane`, reveal) dies
        //       silently. This is the mirror of T351 (engine
        //       restart re-attaching surviving panes): there the
        //       app survives and the engine restarts; here the
        //       engine survives and the app restarts. We require
        //       the old pid to be genuinely dead so a second
        //       live app can't hijack the trust root from the
        //       real one.
        let engine_pid = std::process::id() as libc::pid_t;
        let current_app_pid = server_state.current_app_pid();
        let trust_ok = register_app_session_trust_ok(current_app_pid, peer_pid, engine_pid);
        if !trust_ok {
            tracing::warn!(
                peer_pid = ?peer_pid,
                engine_pid,
                expected_app_pid = ?current_app_pid,
                "register_app_session rejected: peer pid neither matches BOSS_APP_PID nor is an engine ancestor",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "register_app_session: peer pid does not match app_pid".to_owned(),
                },
            );
            return;
        }
        // Re-pin the trust root to the (re)connecting app when it
        // differs from the stale pid. Keeps RPC authorization
        // (`SpawnWorkerPane`, BossOnly/AppOrBoss tiers) following
        // the live app across restarts. Only when a real trust
        // root was configured — test mode (`None`) stays
        // permissive so unit tests aren't pinned to a live pid.
        if let (Some(prior), Some(observed)) = (current_app_pid, peer_pid) {
            if prior != observed {
                server_state.set_app_pid(observed);
                tracing::info!(
                    prior_app_pid = prior,
                    new_app_pid = observed,
                    "app session re-attached: trust root re-pinned to relaunched app",
                );
            }
        }
        server_state
            .register_app_session(session_id.clone(), sink.clone())
            .await;
        tracing::info!(session_id = %session_id, "app session registered");
        send_response(&sink, &request_id, FrontendEvent::AppSessionRegistered);
    }
}

pub(super) async fn handle_register_boss_session(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RegisterBossSession { shell_pid } = req else {
        unreachable!()
    };
    {
        // Only the registered app session may install the
        // Boss trust root.
        let app_session_id = server_state
            .app_session
            .lock()
            .await
            .as_ref()
            .map(|h| h.session_id.clone());
        if app_session_id.as_deref() != Some(session_id.as_str()) {
            tracing::warn!(
                session_id = %session_id,
                "register_boss_session rejected: caller is not the app session",
            );
            send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "register_boss_session: only the app session may install the Boss trust root"
                                .to_owned(),
                        },
                    );
            return;
        }
        server_state.set_boss_pid(shell_pid as libc::pid_t);
        tracing::info!(
            boss_pid = shell_pid,
            "boss session registered as second trust root",
        );
        send_response(&sink, &request_id, FrontendEvent::BossSessionRegistered);
    }
}

pub(super) async fn handle_engine_response(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        session_id,
        ..
    } = ctx;
    let FrontendRequest::EngineResponse {
        request_id: response_request_id,
        response,
    } = req
    else {
        unreachable!()
    };
    {
        server_state
            .deliver_app_response(&session_id, &response_request_id, response)
            .await;
    }
}

pub(super) async fn handle_shutdown(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::Shutdown { token } = req else {
        unreachable!()
    };
    {
        // The token written to disk at startup is the auth
        // credential — there is no pid-based tier check on
        // purpose. The whole point of the token gate (issue
        // #705) is that "same user / same machine" doesn't
        // separate the legitimate caller (macOS app, boss CLI)
        // from the accidental caller (a `bazel test` that
        // resolved the production socket). The bazel sandbox
        // already denies access to `~/Library/Application
        // Support/`, so a test that lands here without the
        // file in scope will fail with `token_missing` rather
        // than killing a 9-hour-old engine.
        let outcome = match server_state.control_token.as_deref() {
            None => {
                // In-process serve() without a control token —
                // shouldn't happen for any process that has a
                // dialable frontend socket, but the dispatcher
                // is the wrong place to assume that. Reject
                // explicitly rather than panic.
                "token_missing"
            }
            Some(expected) => {
                if constant_time_eq(expected.as_bytes(), token.as_bytes()) {
                    "accepted"
                } else {
                    "token_mismatch"
                }
            }
        };
        crate::audit::record_shutdown_rpc(outcome, peer_pid.map(|p| p as i32));
        if outcome == "accepted" {
            tracing::info!(
                peer_pid = ?peer_pid,
                "shutdown rpc: token accepted — graceful exit pending",
            );
            send_response(&sink, &request_id, FrontendEvent::ShutdownAccepted);
            // Defer the actual notify so the writer task has a
            // chance to drain the ShutdownAccepted frame into
            // the kernel socket buffer before the accept loop
            // breaks. 50 ms is well under the shutdown_workers
            // grace window and well over the time it takes the
            // dispatcher to enqueue + the writer task to flush.
            let trigger = server_state.shutdown_trigger.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                trigger.notify_one();
            });
        } else {
            tracing::warn!(
                peer_pid = ?peer_pid,
                outcome,
                "shutdown rpc: rejected",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ShutdownRejected {
                    reason: outcome.to_owned(),
                },
            );
        }
    }
}
