//! `FrontendRequest` handlers — project CRUD, ordering, and design docs.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_list_projects(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListProjects { product_id, dep_filter } = req else {
        unreachable!()
    };
    {
        match work_db.list_projects(&product_id, dep_filter.as_ref()) {
            Ok(projects) => {
                send_response_with_revision(
                    &sink,
                    &request_id,
                    server_state.current_work_revision(),
                    FrontendEvent::ProjectsList { product_id, projects },
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

pub(super) async fn handle_create_project(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateProject { input } = req else {
        unreachable!()
    };
    match work_db.create_project(input) {
        Ok(project) => {
            let item = WorkItem::Project(project);
            let product_id = work_item_product_id(&item);
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![work_product_topic(&product_id)],
                "project_created",
                Some(product_id),
                vec![work_item_id(&item)],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemCreated { item });
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

pub(super) async fn handle_reorder_project_tasks(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ReorderProjectTasks { project_id, task_ids } = req else {
        unreachable!()
    };
    match work_db.get_work_item(&project_id) {
        Ok(project_item) => match work_db.reorder_project_tasks(&project_id, &task_ids) {
            Ok(()) => {
                let product_id = work_item_product_id(&project_item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "project_tasks_reordered",
                    Some(product_id),
                    task_ids.clone(),
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::ProjectTasksReordered { project_id, task_ids },
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
        },
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

pub(super) async fn handle_set_project_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProjectDesignDoc { input } = req else {
        unreachable!()
    };
    {
        match work_db.set_project_design_doc(input) {
            Ok(project) => {
                let item = WorkItem::Project(project);
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "project_design_doc_set",
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

pub(super) async fn handle_resolve_project_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ResolveProjectDesignDoc { project_id } = req else {
        unreachable!()
    };
    {
        // Build a (repo_remote_url -> workspace_path) lookup so the
        // resolver can hand the open dispatcher an absolute
        // workspace path for `$EDITOR` / renderer fast-path.
        // First-match wins when multiple workspaces lease the
        // same repo — any of them resolves the file equally well.
        let leased_repo_paths: HashMap<String, String> = work_db
            .list_in_flight_executions()
            .map(|execs| {
                let mut map = HashMap::new();
                for exec in execs {
                    if let Some(path) = exec.workspace_path
                        && !map.contains_key(&exec.repo_remote_url)
                    {
                        map.insert(exec.repo_remote_url, path);
                    }
                }
                map
            })
            .unwrap_or_default();
        match work_db.resolve_project_design_doc(&project_id, |repo| leased_repo_paths.get(repo).cloned()) {
            Ok(output) => send_response(&sink, &request_id, FrontendEvent::ProjectDesignDocResolved { output }),
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
