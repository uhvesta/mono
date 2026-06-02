//! `FrontendRequest` handlers — attention items and groups.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_create_attention_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateAttentionItem { input } = req else {
        unreachable!()
    };
    {
        match work_db.create_attention_item(input) {
            Ok(item) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionItemCreated { item },
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

pub(super) async fn handle_list_attention_items(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAttentionItems { execution_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_attention_items(&execution_id) {
            Ok(items) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionItemsList {
                        execution_id,
                        items,
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

pub(super) async fn handle_get_attention_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetAttentionItem { id } = req else {
        unreachable!()
    };
    match work_db.get_attention_item(&id) {
        Ok(item) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::AttentionItemResult { item },
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

pub(super) async fn handle_list_attention_items_for_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAttentionItemsForWorkItem { work_item_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_attention_items_for_work_item(&work_item_id) {
            Ok(items) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionItemsForWorkItemList {
                        work_item_id,
                        items,
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

pub(super) async fn handle_list_attention_groups(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAttentionGroups {
        product_id,
        project_id,
        task_id,
        kind,
        state,
    } = req
    else {
        unreachable!()
    };
    {
        let listed = work_db
            .list_attention_groups(
                &product_id,
                project_id.as_deref(),
                task_id.as_deref(),
                kind.as_deref(),
                state.as_deref(),
            )
            .and_then(|groups| {
                // Bundle every group's member rows in one reply so the
                // Notifications window renders inline controls without a
                // round-trip per group. Flattened across groups; the
                // client buckets by `group_id`.
                let mut members = Vec::new();
                for group in &groups {
                    members.extend(work_db.list_attentions_for_group(&group.id)?);
                }
                Ok((groups, members))
            });
        match listed {
            Ok((groups, members)) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionGroupsList {
                        product_id,
                        groups,
                        members,
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

pub(super) async fn handle_get_attention_group(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetAttentionGroup { id } = req else {
        unreachable!()
    };
    {
        let fetched = work_db.get_attention_group(&id).and_then(|group| {
            let members = work_db.list_attentions_for_group(&group.id)?;
            Ok((group, members))
        });
        match fetched {
            Ok((group, members)) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionGroupResult { group, members },
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

pub(super) async fn handle_create_attention(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateAttention { input } = req else {
        unreachable!()
    };
    {
        match work_db.create_attention(input) {
            Ok((attention, group)) => {
                // Live-update the Notifications window + doc viewer on
                // the owning product's work-tree topic.
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &group.product_id,
                        FrontendEvent::AttentionCreated {
                            attention: attention.clone(),
                            group: group.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionCreated { attention, group },
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

pub(super) async fn handle_answer_attention(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AnswerAttention {
        id,
        answer,
        skip,
        dismiss,
    } = req
    else {
        unreachable!()
    };
    match work_db.answer_attention(&id, answer, skip, dismiss) {
        Ok(group) => {
            let members = work_db
                .list_attentions_for_group(&group.id)
                .unwrap_or_default();
            server_state
                .publisher
                .publish_frontend_event_on_product(
                    &group.product_id,
                    FrontendEvent::AttentionGroupUpdated {
                        group: group.clone(),
                        members: members.clone(),
                    },
                )
                .await;
            send_response(
                &sink,
                &request_id,
                FrontendEvent::AttentionGroupUpdated { group, members },
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

pub(super) async fn handle_dismiss_attention(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DismissAttention { id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.dismiss_attention(&id, reason) {
            Ok(group) => {
                let members = work_db
                    .list_attentions_for_group(&group.id)
                    .unwrap_or_default();
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &group.product_id,
                        FrontendEvent::AttentionGroupUpdated {
                            group: group.clone(),
                            members: members.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AttentionGroupUpdated { group, members },
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

pub(super) async fn handle_action_attention_group(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ActionAttentionGroup {
        id,
        skip_unanswered,
    } = req
    else {
        unreachable!()
    };
    match work_db.action_attention_group(&id, skip_unanswered, &GhPrStateChecker) {
        Ok(ActionedAttentionGroup {
            group,
            produced_work_item_ids,
        }) => {
            let members = work_db
                .list_attentions_for_group(&group.id)
                .unwrap_or_default();
            // Live-update the Notifications window + inline doc surface.
            server_state
                .publisher
                .publish_frontend_event_on_product(
                    &group.product_id,
                    FrontendEvent::AttentionGroupActioned {
                        group: group.clone(),
                        members: members.clone(),
                    },
                )
                .await;
            // Refresh the kanban / work tree so the produced revision
            // or tasks appear without a manual reload.
            if !produced_work_item_ids.is_empty() {
                publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&group.product_id)],
                    "attention_group_actioned",
                    Some(group.product_id.clone()),
                    produced_work_item_ids,
                )
                .await;
            }
            send_response(
                &sink,
                &request_id,
                FrontendEvent::AttentionGroupActioned { group, members },
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
