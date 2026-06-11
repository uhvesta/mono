//! `FrontendRequest` handlers — external-tracker config and work-item links.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_set_product_external_tracker(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProductExternalTracker { input } = req else {
        unreachable!()
    };
    {
        let validation_result = if input.unset {
            Ok(())
        } else {
            match (input.kind.as_deref(), input.config.as_ref()) {
                (None, _) | (_, None) => Err("both kind and config must be provided when not using unset".to_owned()),
                (Some(kind), Some(config)) => validate_external_tracker_config(kind, config),
            }
        };
        match validation_result {
            Err(msg) => send_response(&sink, &request_id, FrontendEvent::WorkError { message: msg }),
            Ok(()) => {
                let result = work_db.set_product_external_tracker(
                    &input.product_id,
                    input.kind.as_deref(),
                    input.config.as_ref(),
                    input.unset,
                );
                match result {
                    Ok(product) => {
                        let item = WorkItem::Product(product);
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "external_tracker_updated",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
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
    }
}

pub(super) async fn handle_sync_product_external_tracker(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SyncProductExternalTracker { product_id } = req else {
        unreachable!()
    };
    {
        let work_db = server_state.work_db.clone();
        let registry = server_state.tracker_registry.clone();
        let metrics = server_state.metrics.clone();
        let publisher = server_state.clone();
        let credential_resolver = server_state.tracker_credential_resolver.clone();
        let sink2 = sink.clone();
        let request_id2 = request_id.clone();
        tokio::spawn(async move {
            match crate::external_tracker::reconcile::run_one_pass_for_product(
                work_db.as_ref(),
                registry.as_ref(),
                metrics.as_ref(),
                &product_id,
                publisher.as_ref(),
                credential_resolver.as_ref(),
            )
            .await
            {
                Some(outcome) => {
                    tracing::info!(
                        product_id,
                        items_imported = outcome.items_imported,
                        items_closed = outcome.items_closed,
                        pr_attached = outcome.pr_attached,
                        close_issue_succeeded = outcome.close_issue_succeeded,
                        close_issue_failed = outcome.close_issue_failed,
                        items_unbound = outcome.items_unbound,
                        "on-demand external tracker sync complete",
                    );
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::ExternalTrackerSyncStarted { product_id },
                    );
                }
                None => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::WorkError {
                            message: format!("product '{product_id}' has no external tracker binding"),
                        },
                    );
                }
            }
        });
    }
}

pub(super) async fn handle_link_work_item_external_ref(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::LinkWorkItemExternalRef { input } = req else {
        unreachable!()
    };
    {
        let result = work_db
            .set_external_ref(
                &input.work_item_id,
                &input.kind,
                &input.canonical_id,
                &serde_json::Value::Null,
            )
            .and_then(|()| work_db.get_task_with_external_ref(&input.work_item_id));
        match result {
            Ok(item) => {
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "work_item_updated",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
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

pub(super) async fn handle_unlink_work_item_external_ref(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::UnlinkWorkItemExternalRef {
        work_item_id: target_id,
    } = req
    else {
        unreachable!()
    };
    {
        let result = work_db
            .clear_external_ref(&target_id)
            .and_then(|()| work_db.get_task_with_external_ref(&target_id));
        match result {
            Ok(item) => {
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "work_item_updated",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
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
