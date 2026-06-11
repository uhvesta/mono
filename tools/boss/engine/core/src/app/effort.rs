//! `FrontendRequest` handlers — effort auditing and escalation recording.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_audit_product_effort(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AuditProductEffort {
        product_id,
        window_days,
    } = req
    else {
        unreachable!()
    };
    {
        // Read-only diagnostic surface for `boss product
        // audit-effort`. No auth gate — the rows are the
        // chore corpus the caller can already enumerate
        // via `boss chore list`, and the escalation events
        // are coordinator-emitted facts about that corpus.
        let result = build_effort_audit_report(&work_db, &product_id, window_days);
        match result {
            Ok(report) => send_response(&sink, &request_id, FrontendEvent::EffortAuditReport { report }),
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}

pub(super) async fn handle_record_effort_escalation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RecordEffortEscalation {
        work_item_id,
        original_level,
        new_level,
        markers,
        rule_id,
    } = req
    else {
        unreachable!()
    };
    {
        // Coordinator-only RPC in practice (the sibling
        // escalation-handler task is the only caller in
        // v1), but the engine doesn't gate it — the row
        // is opaque diagnostic data and a forged event is
        // bounded to one false-positive in the audit
        // report.
        match work_db.record_effort_escalation(&work_item_id, original_level, new_level, &markers, rule_id.as_deref()) {
            Ok(event) => send_response(&sink, &request_id, FrontendEvent::EffortEscalationRecorded { event }),
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}
