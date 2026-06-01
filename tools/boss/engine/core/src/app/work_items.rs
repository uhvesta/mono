//! `FrontendRequest` handlers — task/chore/work-item CRUD and queries.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_list_tasks(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListTasks {
        product_id,
        project_id,
        dep_filter,
        include_deleted,
    } = req
    else {
        unreachable!()
    };
    match work_db.list_tasks(
        &product_id,
        project_id.as_deref(),
        dep_filter.as_ref(),
        include_deleted,
    ) {
        Ok(tasks) => {
            send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::TasksList {
                    product_id,
                    project_id,
                    tasks,
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

pub(super) async fn handle_list_chores(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListChores {
        product_id,
        dep_filter,
        include_deleted,
    } = req
    else {
        unreachable!()
    };
    match work_db.list_chores(&product_id, dep_filter.as_ref(), include_deleted) {
        Ok(chores) => {
            send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::ChoresList { product_id, chores },
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

pub(super) async fn handle_get_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetWorkItem { id } = req else {
        unreachable!()
    };
    {
        // Use resolving variant so callers can pass T-form short ids
        // (e.g. `T688`) without knowing the product; the DB lookup is
        // global and short ids are unique across all products.
        let result = work_db
            .get_work_item_resolving_short_id(&id)
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("unknown work item: {id}")));
        match result {
            Ok(item) => {
                send_response_with_revision(
                    &sink,
                    &request_id,
                    server_state.current_work_revision(),
                    FrontendEvent::WorkItemResult { item },
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

pub(super) async fn handle_get_work_item_by_short_id(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetWorkItemByShortId {
        product_id,
        short_id,
    } = req
    else {
        unreachable!()
    };
    match work_db.get_work_item_by_short_id(&product_id, short_id) {
        Ok(Some(item)) => {
            send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::WorkItemResult { item },
            );
        }
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("no work item with id #{short_id} in product {product_id}"),
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

pub(super) async fn handle_find_work_items_by_pr(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::FindWorkItemsByPr { pr_number } = req else {
        unreachable!()
    };
    {
        match work_db.find_work_items_by_pr(pr_number) {
            Ok(matches) => {
                send_response_with_revision(
                    &sink,
                    &request_id,
                    server_state.current_work_revision(),
                    FrontendEvent::WorkItemsByPrResult { pr_number, matches },
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

pub(super) async fn handle_create_task(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateTask { mut input } = req else {
        unreachable!()
    };
    {
        if input.created_via.is_none() {
            input.created_via =
                Some(transport_default_created_via(&server_state, &session_id).await);
        }
        // A `--repo <slug>` override (e.g. `bduff`) names a
        // registered cube repo, not a git URL. Resolve it to the
        // canonical origin now so the durable row is dispatchable
        // and `cube repo ensure` never sees a bare slug (#861).
        repo_slug::resolve_repo_slugs(&server_state.cube_client, &mut [&mut input.repo_remote_url])
            .await;
        match work_db.create_task(input) {
            Ok(task) => {
                let item = WorkItem::Task(task);
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "task_created",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::WorkItemCreated { item },
                );
            }
            Err(err) => {
                send_response(&sink, &request_id, duplicate_or_work_error(err));
            }
        }
    }
}

pub(super) async fn handle_create_chore(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateChore { mut input } = req else {
        unreachable!()
    };
    {
        if input.created_via.is_none() {
            input.created_via =
                Some(transport_default_created_via(&server_state, &session_id).await);
        }
        // Resolve a `--repo <slug>` override to its canonical cube
        // origin before persisting (#861); see the CreateTask arm.
        repo_slug::resolve_repo_slugs(&server_state.cube_client, &mut [&mut input.repo_remote_url])
            .await;
        match work_db.create_chore(input) {
            Ok(task) => {
                let item = WorkItem::Chore(task);
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "chore_created",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::WorkItemCreated { item },
                );
            }
            Err(err) => {
                send_response(&sink, &request_id, duplicate_or_work_error(err));
            }
        }
    }
}

pub(super) async fn handle_create_many_tasks(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateManyTasks { mut input } = req else {
        unreachable!()
    };
    {
        let fallback = transport_default_created_via(&server_state, &session_id).await;
        for item in &mut input.items {
            if item.created_via.is_none() {
                item.created_via = Some(fallback.clone());
            }
        }
        // Resolve any `--repo <slug>` overrides in the batch to
        // canonical cube origins in a single registry round-trip (#861).
        {
            let mut fields: Vec<&mut Option<String>> = input
                .items
                .iter_mut()
                .map(|i| &mut i.repo_remote_url)
                .collect();
            repo_slug::resolve_repo_slugs(&server_state.cube_client, &mut fields).await;
        }
        handle_create_many(
            work_db.create_many_tasks(input),
            "tasks_created",
            WorkItem::Task,
            &server_state,
            &session_id,
            &request_id,
            &sink,
        )
        .await;
    }
}

pub(super) async fn handle_create_many_chores(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateManyChores { mut input } = req else {
        unreachable!()
    };
    {
        let fallback = transport_default_created_via(&server_state, &session_id).await;
        for item in &mut input.items {
            if item.created_via.is_none() {
                item.created_via = Some(fallback.clone());
            }
        }
        // Resolve any `--repo <slug>` overrides in the batch to
        // canonical cube origins in a single registry round-trip (#861).
        {
            let mut fields: Vec<&mut Option<String>> = input
                .items
                .iter_mut()
                .map(|i| &mut i.repo_remote_url)
                .collect();
            repo_slug::resolve_repo_slugs(&server_state.cube_client, &mut fields).await;
        }
        handle_create_many(
            work_db.create_many_chores(input),
            "chores_created",
            WorkItem::Chore,
            &server_state,
            &session_id,
            &request_id,
            &sink,
        )
        .await;
    }
}

pub(super) async fn handle_update_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        peer_pid,
    } = ctx;
    let FrontendRequest::UpdateWorkItem { id, patch } = req else {
        unreachable!()
    };
    {
        // Capture the task/chore status before the update so we
        // can detect a transition into `active` after the patch
        // applies. We only care about task/chore — products and
        // projects have no execution lifecycle.
        let previous_task_status = task_status_for_id(&work_db, &id);
        // Capture name+description before the update so the
        // chore-update worker notification can report old → new.
        // Only read when the patch touches these fields to avoid
        // an unconditional DB round-trip on status-only patches.
        let previous_spec = if patch.name.is_some() || patch.description.is_some() {
            task_name_description_for_id(&work_db, &id)
        } else {
            None
        };
        // Bug #679: when the patch is a kanban drag-to-Doing
        // (a task/chore transitioning from non-active to
        // `active`) and dispatch would deterministically fail
        // because the row has no resolvable repo, reject the
        // `UpdateWorkItem` outright instead of letting the
        // status flip land and then swallowing the dispatch
        // error in a `WARN`. The card stays in its previous
        // column and the user sees a `WorkError` toast naming
        // the missing repo. Skips when an existing non-terminal
        // execution would already own the dispatch slot —
        // there's no point validating a code path we won't run.
        let intends_active_transition = patch.status.as_deref() == Some("active")
            && previous_task_status
                .as_deref()
                .is_some_and(|prev| prev != "active");
        if intends_active_transition && work_item_needs_dispatch(&work_db, &id) {
            if let Err(err) = work_db.precheck_dispatch_repo(&id) {
                let work_item_id_for_event = id.clone();
                let from_status = previous_task_status.clone();
                let error_message = format!("{err:#}");
                let details = serde_json::json!({
                    "from_status": from_status,
                    "to_status": "active",
                    "did_dispatch": false,
                    "rejected": true,
                    "reason_if_skipped": error_message,
                    "dispatched_execution_id": serde_json::Value::Null,
                });
                server_state
                    .dispatch_events
                    .emit(
                        crate::dispatch_events::DispatchEvent::new(
                            crate::dispatch_events::Stage::StatusTransition,
                            crate::dispatch_events::Outcome::Error,
                            work_item_id_for_event.clone(),
                        )
                        .with_work_item(work_item_id_for_event)
                        .with_error(&err)
                        .with_details(details),
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
                return;
            }
        }
        let actor = resolve_status_actor(&server_state, peer_pid);
        match work_db.update_work_item_as_actor(&id, patch, actor) {
            Ok(item) => {
                let product_id = work_item_product_id(&item);
                let mut topics = vec![work_product_topic(&product_id)];
                if matches!(item, WorkItem::Product(_)) {
                    topics.push(TOPIC_WORK_PRODUCTS.to_owned());
                }
                // If the patch moved a task/chore into a
                // terminal status (`done`, `archived`, or
                // `cancelled`), tear down whatever resources
                // its latest execution still holds: the
                // libghostty pane and the cube workspace.
                // Idempotent — duplicate or no-op cases
                // (already released, never spawned, not a
                // task/chore) collapse inside force_release.
                if let Some(execution_id) = terminal_chore_execution(&work_db, &item) {
                    let handler = server_state.completion_handler.clone();
                    tokio::spawn(async move {
                        handler.force_release(&execution_id).await;
                    });
                }
                // If the patch moved a task/chore into
                // `in_review`, release pane + cube workspace
                // for the same reason — the worker is done
                // with the slot. The worker auto-transition
                // path (Stop hook → finalize_pr_transition)
                // handles its own release; this block covers
                // the human-drag path and any ghost panes left
                // behind by a failed or partial auto-release.
                // Idempotent for the same reasons as above.
                if let Some(execution_id) = in_review_chore_execution(&work_db, &item) {
                    let handler = server_state.completion_handler.clone();
                    tokio::spawn(async move {
                        handler.force_release(&execution_id).await;
                    });
                }
                // If the user dragged an active task/chore back
                // to Backlog (active → todo), stop the live
                // worker: cancel its execution row (so the orphan
                // sweep and reconciler won't re-dispatch it) and
                // release its pane + cube workspace. The task
                // status is already `todo` from the patch above;
                // autostart was cleared to 0 when the task first
                // entered Doing, so it will not be re-dispatched.
                if let Some(execution_id) =
                    active_to_todo_execution(&work_db, &previous_task_status, &item)
                {
                    let handler = server_state.completion_handler.clone();
                    tokio::spawn(async move {
                        handler.cancel_and_release(&execution_id).await;
                    });
                }
                // Kanban drop-into-Doing (and any other human
                // path that flips a task/chore to `active` via
                // UpdateWorkItem) must dispatch a worker — see
                // `tools/boss/docs/designs/work-kanban.md` §
                // "Doing column = live or queued". The macOS
                // client also fires `RequestExecution` after
                // the status patch, but doing it server-side
                // closes the gap for older clients (or any
                // future client that forgets the follow-up
                // RPC), which is the failure shape the
                // motivating bug exposed for `autostart=false`
                // chores parked in `todo`: the autostart gate
                // blocks creation-time dispatch, so until the
                // human drags the card there is no execution
                // at all, and a status flip with no follow-up
                // RequestExecution leaves an `active` card
                // with no worker.
                //
                // We only create a fresh execution when the
                // work item has no live/queued one — an
                // existing non-terminal execution already owns
                // the dispatch slot, and replacing it would
                // race the auto-dispatcher (and would void the
                // execution id the client is already tracking).
                // The reconcile / rescan paths handle
                // re-dispatch of stale (worker-died) cases.
                if task_transitioned_to_active(&previous_task_status, &item) {
                    let work_item_id_for_event = work_item_id(&item);
                    let from_status = previous_task_status.clone();
                    let needs_dispatch =
                        work_item_needs_dispatch(&work_db, &work_item_id_for_event);
                    let (dispatched_execution_id, did_dispatch, skip_reason) = if needs_dispatch {
                        let live_states = server_state.live_worker_states.clone();
                        let dispatch_input = RequestExecutionInput::builder()
                            .work_item_id(work_item_id_for_event.clone())
                            .build();
                        match work_db.request_execution_with_live_check(dispatch_input, |run_id| {
                            live_states.is_run_live(run_id)
                        }) {
                            Ok(execution) => {
                                server_state.execution_coordinator.kick();
                                (Some(execution.id), true, None)
                            }
                            Err(err) => {
                                // Deterministic preconditions (no
                                // resolvable repo, bug #679) are
                                // caught by the pre-update
                                // `precheck_dispatch_repo` gate above
                                // and reject the patch outright. This
                                // arm now only fires for non-
                                // deterministic races (e.g., a
                                // concurrent execution insert lost
                                // the unique-row gate). Keep the WARN
                                // so a residual silent skip is still
                                // observable in engine-trace.jsonl.
                                tracing::warn!(
                                    work_item_id = %work_item_id_for_event,
                                    ?err,
                                    "UpdateWorkItem → active: auto-dispatch \
                                     failed; status update kept, no worker spawned",
                                );
                                (None, false, Some(format!("{err:#}")))
                            }
                        }
                    } else {
                        // The auto-dispatch gate decided this transition
                        // already has an in-flight execution. Before this
                        // event existed the skip was silent — exactly the
                        // "I dragged it and nothing happened" shape.
                        (
                            None,
                            false,
                            Some(
                                "work_item_needs_dispatch=false (existing \
                                             non-terminal execution owns dispatch slot)"
                                    .to_owned(),
                            ),
                        )
                    };
                    // Pin the event's execution_id to the resolved exec id
                    // when dispatch landed, falling back to the work item
                    // id otherwise so the line stays correlatable with
                    // anything the operator can grep for.
                    let exec_for_event = dispatched_execution_id
                        .clone()
                        .unwrap_or_else(|| work_item_id_for_event.clone());
                    let details = serde_json::json!({
                        "from_status": from_status,
                        "to_status": "active",
                        "did_dispatch": did_dispatch,
                        "reason_if_skipped": skip_reason,
                        "dispatched_execution_id": dispatched_execution_id,
                    });
                    server_state
                        .dispatch_events
                        .emit(
                            crate::dispatch_events::DispatchEvent::new(
                                crate::dispatch_events::Stage::StatusTransition,
                                if did_dispatch {
                                    crate::dispatch_events::Outcome::Ok
                                } else {
                                    crate::dispatch_events::Outcome::Skipped
                                },
                                exec_for_event,
                            )
                            .with_work_item(work_item_id_for_event)
                            .with_details(details),
                        )
                        .await;
                }
                // If the name or description of an active chore
                // changed, notify the bound worker. The worker may
                // be mid-flight on the old spec; this notice lets it
                // adapt without a human manually sending the update.
                // Fire-and-forget: a failed send (worker pane gone,
                // app session not registered) must not roll back the
                // DB update. Two rapid edits may produce two notices
                // in sequence — that's acceptable per the acceptance
                // criteria.
                if let Some((old_name, old_description)) = previous_spec {
                    if let Some(run_id) = active_chore_run_id(&server_state, &item) {
                        let (new_name, new_description) = match &item {
                            WorkItem::Task(t) | WorkItem::Chore(t) => {
                                (t.name.clone(), t.description.clone())
                            }
                            _ => unreachable!(
                                "active_chore_run_id only returns Some for tasks/chores"
                            ),
                        };
                        if let Some(msg) = build_chore_update_message(
                            &old_name,
                            &new_name,
                            &old_description,
                            &new_description,
                        ) {
                            let server_for_notify = server_state.clone();
                            tokio::spawn(async move {
                                if let Err(err) =
                                    server_for_notify.send_input_to_worker(&run_id, msg).await
                                {
                                    tracing::warn!(
                                        ?err,
                                        %run_id,
                                        "chore-update: failed to notify live worker",
                                    );
                                }
                            });
                        }
                    }
                }
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    topics,
                    "work_item_updated",
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

pub(super) async fn handle_delete_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DeleteWorkItem { id } = req else {
        unreachable!()
    };
    match work_db.get_work_item(&id) {
        Ok(item) => match work_db.delete_work_item(&id) {
            Ok(()) => {
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "work_item_deleted",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::WorkItemDeleted { id },
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

pub(super) async fn handle_restore_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RestoreWorkItem { id } = req else {
        unreachable!()
    };
    match work_db.restore_work_item(&id) {
        Ok(item) => {
            // Restore makes a tombstoned row live again, so the
            // kanban / list consumers need to reload exactly as
            // they would for any other mutation. Reuse the
            // `work_item_updated` invalidation rather than minting
            // a restore-specific topic event.
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
            send_response_with_revision(
                &sink,
                &request_id,
                revision,
                FrontendEvent::WorkItemRestored { item },
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

pub(super) async fn handle_get_work_tree(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetWorkTree { product_id } = req else {
        unreachable!()
    };
    match work_db.get_work_tree(&product_id) {
        Ok(tree) => {
            send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::WorkTree {
                    product: tree.product,
                    projects: tree.projects,
                    tasks: tree.tasks,
                    chores: tree.chores,
                    task_runtimes: tree.task_runtimes,
                    dependencies: tree.dependencies,
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

pub(super) async fn handle_reveal_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RevealWorkItem { id } = req else {
        unreachable!()
    };
    {
        // `bossctl reveal` is a coordinator verb for navigating
        // the macOS app to a specific work item's card. Same
        // authority tier as `focus_worker_pane` — it's a UI
        // steering RPC invoked from the Boss pane or app shell.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                id = %id,
                "reveal_work_item rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "reveal_work_item requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.reveal_work_item(&id).await {
            Ok(canonical_id) => {
                tracing::info!(
                    id = %id,
                    canonical_id = %canonical_id,
                    "reveal_work_item: card highlighted",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkItemRevealed { id: canonical_id },
                );
            }
            Err(err) => {
                tracing::warn!(?err, id = %id, "reveal_work_item failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("reveal_work_item: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_create_investigation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateInvestigation { input } = req else {
        unreachable!()
    };
    {
        match work_db.create_investigation(input) {
            Ok(task) => {
                let item = WorkItem::Task(task);
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "investigation_created",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::WorkItemCreated { item },
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

pub(super) async fn handle_create_revision(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateRevision { input } = req else {
        unreachable!()
    };
    {
        match work_db.create_revision(input, &GhPrStateChecker) {
            Ok(task) => {
                let item = WorkItem::Task(task);
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "revision_created",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::WorkItemCreated { item },
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
