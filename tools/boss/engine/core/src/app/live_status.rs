//! `FrontendRequest` handlers — live-status enable/disable and pipeline debug.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_set_live_status_enabled(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetLiveStatusEnabled { slot_id, enabled } = req else {
        unreachable!()
    };
    {
        server_state.live_status_manager.set_enabled(slot_id, enabled);
        if let Err(err) =
            persist_live_status_disabled_slots(&work_db, &server_state.live_status_manager.disabled_snapshot())
        {
            tracing::warn!(
                slot_id,
                enabled,
                ?err,
                "live_status: failed to persist disabled-slot toggle",
            );
        }
        send_response(
            &sink,
            &request_id,
            FrontendEvent::LiveStatusEnabledSet { slot_id, enabled },
        );
    }
}

pub(super) async fn handle_list_live_status_disabled_slots(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListLiveStatusDisabledSlots = req else {
        unreachable!()
    };
    {
        let slot_ids = server_state.live_status_manager.disabled_snapshot();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::LiveStatusDisabledSlotsList { slot_ids },
        );
    }
}

pub(super) async fn handle_debug_live_status_pipeline(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DebugLiveStatusPipeline = req else {
        unreachable!()
    };
    {
        let report = build_live_status_debug_report(&server_state, &work_db);
        send_response(&sink, &request_id, FrontendEvent::LiveStatusDebugReportEvent { report });
    }
}
