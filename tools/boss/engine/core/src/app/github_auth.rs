//! `FrontendRequest` handlers — GitHub OAuth device-flow controls.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_git_hub_auth_start(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthStart = req else {
        unreachable!()
    };
    {
        // Begin (or restart) the device flow. The controller transitions
        // to `RequestingCode` synchronously and drives the rest in a
        // background task; the forwarder pushes each subsequent state on
        // the `github.auth` topic. Reply with the immediate state so the
        // request completes.
        server_state.github_auth.start_flow().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_cancel(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthCancel = req else {
        unreachable!()
    };
    {
        server_state.github_auth.cancel().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_disconnect(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthDisconnect = req else {
        unreachable!()
    };
    {
        // Deletes the keychain token and drops to `Disconnected`.
        server_state.github_auth.disconnect().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthStatus = req else {
        unreachable!()
    };
    {
        let current = server_state.github_auth.current_state();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: current.to_dto(),
            },
        );
        // "Re-check" (design §7): when connected, re-run the org/SSO
        // probe so an org-approval / SSO banner clears on its own once
        // the owner approves or the user SSO-authorizes — no full
        // re-auth needed. `update_org_state` only notifies on a real
        // change, which the forwarder then pushes on `github.auth`.
        if let GitHubAuthState::Authorized { record, .. } = current {
            let server_state = server_state.clone();
            let token = record.token.clone();
            tokio::spawn(async move {
                let controller = server_state.github_auth.clone();
                let flow = controller.device_flow();
                let resolved = probe_and_record_org_state(
                    server_state.work_db.as_ref(),
                    flow.as_ref(),
                    &token,
                )
                .await;
                controller.update_org_state(resolved);
            });
        }
    }
}
