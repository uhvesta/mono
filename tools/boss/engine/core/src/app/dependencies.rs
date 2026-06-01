//! `FrontendRequest` handlers — work-item dependency edges.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_add_dependency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddDependency { input } = req else {
        unreachable!()
    };
    {
        match work_db.add_dependency(input) {
            Ok(edge) => {
                // Edge changes don't move any work item's status
                // in this PR (status mechanics arrive in the
                // follow-up phase), but we still publish a
                // work-invalidation so subscribers re-render the
                // dependency surfaces (kanban badge, show view).
                let product_id = match work_db.get_work_item(&edge.dependent_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(_) => None,
                };
                let revision = if let Some(pid) = product_id.as_deref() {
                    publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(pid)],
                        "dependency_added",
                        Some(pid.to_owned()),
                        vec![edge.dependent_id.clone(), edge.prerequisite_id.clone()],
                    )
                    .await
                } else {
                    server_state.current_work_revision()
                };
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::DependencyAdded { edge },
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

pub(super) async fn handle_remove_dependency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveDependency { input } = req else {
        unreachable!()
    };
    {
        let dependent_id = input.dependent.clone();
        let prerequisite_id = input.prerequisite.clone();
        let relation = input
            .relation
            .clone()
            .unwrap_or_else(|| "blocks".to_owned());
        match work_db.remove_dependency(input) {
            Ok(removed) => {
                let product_id = match work_db.get_work_item(&dependent_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(_) => None,
                };
                let revision = if let Some(pid) = product_id.as_deref() {
                    publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(pid)],
                        "dependency_removed",
                        Some(pid.to_owned()),
                        vec![dependent_id.clone(), prerequisite_id.clone()],
                    )
                    .await
                } else {
                    server_state.current_work_revision()
                };
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::DependencyRemoved {
                        dependent_id,
                        prerequisite_id,
                        relation,
                        removed,
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

pub(super) async fn handle_list_dependencies(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListDependencies { input } = req else {
        unreachable!()
    };
    match work_db.list_dependencies(input) {
        Ok(view) => send_response(&sink, &request_id, FrontendEvent::DependencyList { view }),
        Err(err) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: err.to_string(),
            },
        ),
    }
}

pub(super) async fn handle_list_dependencies_detailed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListDependenciesDetailed { input } = req else {
        unreachable!()
    };
    {
        match work_db.list_dependencies_detailed(input) {
            Ok(detail) => send_response(
                &sink,
                &request_id,
                FrontendEvent::DependencyDetail { detail },
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
