//! `FrontendRequest` handlers — merge-when-ready and review terminals.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_merge_when_ready(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MergeWhenReady { work_item_id } = req else {
        unreachable!()
    };
    {
        // Pre-flight: task must exist and be a Task/Chore.
        let item = match work_db.get_work_item(&work_item_id) {
            Ok(item) => item,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("merge_when_ready: unknown work item: {err}"),
                    },
                );
                return;
            }
        };
        let (pr_url, task_status) = match &item {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                (task.pr_url.clone(), task.status.clone())
            }
            _ => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "merge_when_ready: only supported for tasks/chores".to_owned(),
                    },
                );
                return;
            }
        };
        if task_status != TaskStatus::InReview {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!(
                        "merge_when_ready: task is not in review (status: {task_status})"
                    ),
                },
            );
            return;
        }
        let pr_url = match pr_url.filter(|s| !s.is_empty()) {
            Some(u) => u,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "merge_when_ready: task has no PR URL".to_owned(),
                    },
                );
                return;
            }
        };
        // Spawn the GitHub interaction so the main loop isn't blocked.
        let sink2 = sink.clone();
        let request_id2 = request_id.clone();
        let work_item_id2 = work_item_id.clone();
        let pr_url2 = pr_url.clone();
        let kick = server_state.pr_reconciler_kick.clone();
        tokio::spawn(async move {
            match merge_when_ready::gh_merge_when_ready(&pr_url2).await {
                Ok(action) => {
                    // Kick the PR reconciler so the kanban state
                    // reflects the new merge-queue / auto-merge
                    // state promptly without waiting for the next
                    // periodic sweep.
                    kick.notify_one();
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::MergeWhenReadyAccepted {
                            work_item_id: work_item_id2,
                            pr_url: pr_url2,
                            action: action.as_str().to_owned(),
                        },
                    );
                }
                Err(err) => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::WorkError {
                            message: format!("merge_when_ready failed: {err:#}"),
                        },
                    );
                }
            }
        });
    }
}

pub(super) async fn handle_open_review_terminal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::OpenReviewTerminal { work_item_id } = req else {
        unreachable!()
    };
    {
        let item = match work_db.get_work_item(&work_item_id) {
            Ok(item) => item,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("open_review_terminal: unknown work item: {err}"),
                    },
                );
                return;
            }
        };
        let (pr_url, product_id, task_repo_url) = match &item {
            WorkItem::Task(task) | WorkItem::Chore(task) => (
                task.pr_url.clone(),
                task.product_id.clone(),
                task.repo_remote_url.clone(),
            ),
            _ => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal only supports tasks/chores".to_owned(),
                    },
                );
                return;
            }
        };
        let pr_url = match pr_url.filter(|s| !s.is_empty()) {
            Some(u) => u,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal: task has no PR URL".to_owned(),
                    },
                );
                return;
            }
        };
        let product = match work_db.get_product(&product_id).ok().flatten() {
            Some(p) => p,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("open_review_terminal: unknown product: {product_id}"),
                    },
                );
                return;
            }
        };
        let repo_remote_url = match task_repo_url
            .filter(|s| !s.is_empty())
            .or_else(|| product.repo_remote_url.filter(|s| !s.is_empty()))
        {
            Some(url) => url,
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: "open_review_terminal: task has no repo URL".to_owned(),
                    },
                );
                return;
            }
        };
        let cube_client = server_state.cube_client.clone();
        let sink2 = sink.clone();
        let request_id2 = request_id.clone();
        let work_item_id2 = work_item_id.clone();
        tokio::spawn(async move {
            match open_review_terminal_async(
                &cube_client,
                &repo_remote_url,
                &pr_url,
                &work_item_id2,
            )
            .await
            {
                Ok((workspace_path, lease_id)) => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::ReviewTerminalReady {
                            work_item_id: work_item_id2,
                            workspace_path,
                            lease_id,
                        },
                    );
                }
                Err(err) => {
                    send_response(
                        &sink2,
                        &request_id2,
                        FrontendEvent::WorkError {
                            message: format!("open_review_terminal failed: {err:#}"),
                        },
                    );
                }
            }
        });
    }
}

pub(super) async fn handle_release_review_terminal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch { server_state, .. } = ctx;
    let FrontendRequest::ReleaseReviewTerminal { lease_id } = req else {
        unreachable!()
    };
    {
        let cube_client = server_state.cube_client.clone();
        tokio::spawn(async move {
            if let Err(err) = cube_client.release_workspace(&lease_id).await {
                tracing::warn!(
                    %lease_id,
                    ?err,
                    "release_review_terminal: workspace release failed"
                );
            }
        });
        // fire-and-forget: no reply sent
    }
}
