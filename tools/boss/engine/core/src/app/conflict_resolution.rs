//! `FrontendRequest` handlers — merge-conflict resolution attempts.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_list_conflict_resolutions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListConflictResolutions {
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    {
        // Read-only listing surface for `boss engine conflicts
        // list`. No auth gate — the rows are diagnostic and the
        // caller can already read the SQLite file.
        match work_db.list_conflict_resolutions(
            product_id.as_deref(),
            &status,
            work_item_id.as_deref(),
            limit,
        ) {
            Ok(attempts) => send_response(
                &sink,
                &request_id,
                FrontendEvent::ConflictResolutionsList { attempts },
            ),
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

pub(super) async fn handle_get_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetConflictResolution { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.get_conflict_resolution(&attempt_id) {
            Ok(Some(attempt)) => send_response(
                &sink,
                &request_id,
                FrontendEvent::ConflictResolution { attempt },
            ),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("conflict resolution attempt {attempt_id:?} is unknown",),
                },
            ),
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

pub(super) async fn handle_retry_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RetryConflictResolution { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.retry_conflict_resolution(&attempt_id) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    "retry_conflict_resolution: attempt reset to pending",
                );
                // Mirror the freshly-pending start so the macOS
                // app's activity feed shows the retry as a new
                // attempt. The wire shape is identical to the
                // detection-path's started event — the consumer
                // doesn't need to distinguish.
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::ConflictResolutionStarted {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ConflictResolutionRetried { attempt },
                );
            }
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "conflict resolution attempt {attempt_id:?} is unknown or not in a terminal-failure state (only failed/abandoned rows can be retried)",
                        ),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_abandon_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AbandonConflictResolution { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_conflict_resolution_abandoned(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "abandon_conflict_resolution: attempt flipped to abandoned",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::ConflictResolutionAbandoned {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ConflictResolutionMarkedAbandoned { attempt },
                );
            }
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "conflict resolution attempt {attempt_id:?} is unknown or already terminal",
                        ),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_mark_conflict_resolution_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkConflictResolutionFailed { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        // Worker-facing stop-condition surface. `User` tier:
        // the worker pane invokes `boss engine conflicts
        // mark-failed`, which descends from a worker pane and
        // therefore wouldn't pass `AppOrBoss`. The only state
        // change is on a `conflict_resolutions` row keyed by
        // an opaque id — a worker forging an attempt id has
        // no row to clobber, so authority gates aren't
        // load-bearing here.
        match work_db.mark_conflict_resolution_failed(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "mark_conflict_resolution_failed: attempt flipped to failed",
                );
                // Phase 4 #12: broadcast the typed activity-feed
                // event so subscribers (the macOS app) can
                // render the failed-attempt entry without
                // round-tripping through the CLI's response.
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::ConflictResolutionFailed {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ConflictResolutionMarkedFailed { attempt },
                );
            }
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "conflict resolution attempt {attempt_id:?} is unknown or already terminal",
                        ),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}
