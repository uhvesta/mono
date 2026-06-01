//! `FrontendRequest` handlers — worker pane focus/input/interrupt and live states.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_focus_worker_pane(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::FocusWorkerPane { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents focus` is a coordinator verb that
        // raises a sibling worker pane to the front. The
        // human invokes it from wherever they are — boss
        // pane, app shell, or another worker pane — so the
        // tier is `AppOrBoss`, matching `probe_run` /
        // `stop_run` (which are also legal from inside a
        // worker pane).
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "focus_worker_pane rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "focus_worker_pane requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.focus_worker_pane(&run_id).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "focus_worker_pane: pane raised",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerPaneFocused { run_id, slot_id },
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "focus_worker_pane failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("focus_worker_pane: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_send_input_to_worker(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SendInputToWorker { run_id, text } = req else {
        unreachable!()
    };
    {
        // `bossctl agents send` writes user-typed input into a
        // sibling worker pane. Same authority story as
        // `focus_worker_pane` / `probe_run` / `stop_run`: the
        // human invokes this from wherever they are (boss
        // pane, app shell, or another worker pane), so the
        // tier is `AppOrBoss` — caller must descend from the
        // app or the Boss session.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "send_input_to_worker rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "send_input_to_worker requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.send_input_to_worker(&run_id, text).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "send_input_to_worker: text injected",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerInputSent { run_id, slot_id },
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "send_input_to_worker failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("send_input_to_worker: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_interrupt_worker_pane(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::InterruptWorkerPane { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents interrupt` mirrors the keyboard Esc
        // a human would press inside the worker pane. Same
        // tier rationale as `focus_worker_pane`: the human
        // may invoke it from the Boss pane, the app shell,
        // or a sibling worker pane — `AppOrBoss` admits all
        // three.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "interrupt_worker_pane rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "interrupt_worker_pane requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.interrupt_worker_pane(&run_id).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "interrupt_worker_pane: esc delivered",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerPaneInterrupted { run_id, slot_id },
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "interrupt_worker_pane failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("interrupt_worker_pane: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_worker_live_states(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListWorkerLiveStates = req else {
        unreachable!()
    };
    {
        let states = server_state.live_worker_states_snapshot();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkerLiveStatesList { states },
        );
    }
}
