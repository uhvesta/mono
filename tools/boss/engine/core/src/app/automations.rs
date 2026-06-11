//! `FrontendRequest` handlers — automation CRUD, runs, and triage tasks.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_create_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateAutomation { input } = req else {
        unreachable!()
    };
    {
        match work_db.create_automation(input) {
            Ok(automation) => {
                server_state.automation_scheduler_kick.notify_one();
                send_response(&sink, &request_id, FrontendEvent::AutomationCreated { automation });
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

pub(super) async fn handle_list_automations(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAutomations { product_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_automations_with_open_task_counts(&product_id) {
            Ok(rows) => {
                let open_task_counts = rows.iter().map(|(a, count)| (a.id.clone(), *count)).collect();
                let automations = rows.into_iter().map(|(a, _)| a).collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AutomationsList {
                        product_id,
                        automations,
                        open_task_counts,
                    },
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

pub(super) async fn handle_get_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetAutomation { id } = req else {
        unreachable!()
    };
    {
        match work_db.get_automation(&id) {
            Ok(Some(automation)) => send_response(&sink, &request_id, FrontendEvent::AutomationResult { automation }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("unknown automation: {id}"),
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

pub(super) async fn handle_update_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::UpdateAutomation { id, patch } = req else {
        unreachable!()
    };
    {
        match work_db.update_automation(&id, patch) {
            Ok(automation) => {
                server_state.automation_scheduler_kick.notify_one();
                send_response(&sink, &request_id, FrontendEvent::AutomationUpdated { automation })
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

pub(super) async fn handle_enable_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::EnableAutomation { id } = req else {
        unreachable!()
    };
    {
        match work_db.enable_automation(&id) {
            Ok(automation) => {
                server_state.automation_scheduler_kick.notify_one();
                send_response(&sink, &request_id, FrontendEvent::AutomationUpdated { automation })
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

pub(super) async fn handle_disable_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DisableAutomation { id } = req else {
        unreachable!()
    };
    {
        match work_db.disable_automation(&id) {
            Ok(automation) => {
                server_state.automation_scheduler_kick.notify_one();
                send_response(&sink, &request_id, FrontendEvent::AutomationUpdated { automation })
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

pub(super) async fn handle_delete_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DeleteAutomation { id } = req else {
        unreachable!()
    };
    {
        match work_db.delete_automation(&id) {
            Ok(()) => {
                server_state.automation_scheduler_kick.notify_one();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AutomationDeleted { automation_id: id },
                )
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

pub(super) async fn handle_get_automation_open_task_count(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetAutomationOpenTaskCount { automation_id } = req else {
        unreachable!()
    };
    {
        match work_db.count_open_tasks_for_automation(&automation_id) {
            Ok(count) => send_response(
                &sink,
                &request_id,
                FrontendEvent::AutomationOpenTaskCount { automation_id, count },
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

pub(super) async fn handle_list_editorial_actions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListEditorialActions { product_id, limit } = req else {
        unreachable!()
    };
    {
        match work_db.list_editorial_actions(&product_id, limit, None) {
            Ok(actions) => send_response(
                &sink,
                &request_id,
                FrontendEvent::EditorialActionsList { product_id, actions },
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

pub(super) async fn handle_list_automation_runs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAutomationRuns { automation_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_automation_runs(&automation_id) {
            Ok(runs) => send_response(
                &sink,
                &request_id,
                FrontendEvent::AutomationRunsList { automation_id, runs },
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

pub(super) async fn handle_list_automation_tasks(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAutomationTasks { automation_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_tasks_for_automation(&automation_id) {
            Ok(tasks) => send_response(
                &sink,
                &request_id,
                FrontendEvent::AutomationTasksList { automation_id, tasks },
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

pub(super) async fn handle_run_automation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RunAutomation { automation_id, force } = req else {
        unreachable!()
    };
    {
        // Manual out-of-schedule triage fire (`boss automation run`).
        // Respects the open-task cap unless `force`. Mirrors the
        // scheduler's fire path: dispatch a triage execution, then
        // record an `automation_runs` row for the occurrence (using
        // `now` as `scheduled_for`) WITHOUT advancing the cron schedule
        // (`next_due_at` is left untouched — this is out of band).
        let automation = match work_db.get_automation(&automation_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown automation: {automation_id}"),
                    },
                );
                return;
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
                return;
            }
        };

        if !force {
            match work_db.count_open_tasks_for_automation(&automation_id) {
                Ok(open) if open >= automation.open_task_limit => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "automation {automation_id} is at its open-task limit \
                                         ({open}/{}); pass --force to fire anyway",
                                automation.open_task_limit
                            ),
                        },
                    );
                    return;
                }
                Ok(_) => {}
                Err(err) => {
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
        }

        let coord = server_state.execution_coordinator.clone();
        let dispatcher =
            crate::automation_triage::EngineTriageDispatcher::new(work_db.clone(), Arc::new(move || coord.kick()));
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        match dispatcher.fire(&automation) {
            crate::automation_scheduler::TriageDispatch::Dispatched { execution_id } => {
                if let Err(err) = work_db.record_automation_run_and_advance(
                    crate::work::AutomationFireRecord::builder()
                        .automation_id(automation_id.clone())
                        .scheduled_for(now_epoch)
                        .started_at(now_epoch)
                        .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                        .triage_execution_id(execution_id)
                        .build(),
                ) {
                    tracing::warn!(
                        automation_id = %automation_id,
                        ?err,
                        "manual automation run: triage dispatched but failed to record run row",
                    );
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AutomationRunEnqueued { automation_id },
                );
            }
            crate::automation_scheduler::TriageDispatch::TransientFailure { detail } => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("could not enqueue triage: {detail}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_create_automation_task(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateAutomationTask {
        automation_id,
        name,
        description,
    } = req
    else {
        unreachable!()
    };
    {
        // The triage agent's `boss task create --automation`. Creates
        // the single produced task (with a transactional open-task-cap
        // re-check as the fan-out backstop), then — because the task is
        // `autostart` — requests its execution, which the dispatcher
        // routes to the automations pool on `source_automation_id`.
        match work_db.create_automation_task(&automation_id, &name, description.as_deref()) {
            Ok(task) => {
                let item = WorkItem::Chore(task);
                let work_item_id_for_dispatch = work_item_id(&item);
                let live_states = server_state.live_worker_states.clone();
                let dispatch_input = RequestExecutionInput::builder()
                    .work_item_id(work_item_id_for_dispatch.clone())
                    .build();
                match work_db
                    .request_execution_with_live_check(dispatch_input, |run_id| live_states.is_run_live(run_id))
                {
                    Ok(_execution) => {
                        server_state.execution_coordinator.kick();
                    }
                    Err(err) => {
                        tracing::warn!(
                            work_item_id = %work_item_id_for_dispatch,
                            ?err,
                            "CreateAutomationTask: task created but auto-dispatch failed; \
                             task will start when re-scanned",
                        );
                    }
                }
                let product_id = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "automation_task_created",
                    Some(product_id),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemCreated { item });
            }
            Err(err) => {
                send_response(&sink, &request_id, duplicate_or_work_error(err));
            }
        }
    }
}
