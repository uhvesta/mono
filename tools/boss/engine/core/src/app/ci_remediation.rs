//! `FrontendRequest` handlers — CI remediation attempts and budget.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_classify_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ClassifyCiRemediation {
        attempt_id,
        triage_class,
    } = req
    else {
        unreachable!()
    };
    {
        // Worker-facing marker: stamp `triage_class` on a
        // `ci_remediations` row. Pure metadata column, no
        // authority gate — a forged attempt id has no row to
        // clobber.
        match work_db.set_ci_remediation_triage_class(&attempt_id, &triage_class) {
            Ok(Some(attempt)) => send_response(&sink, &request_id, FrontendEvent::CiRemediationClassified { attempt }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown",),
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

pub(super) async fn handle_mark_ci_remediation_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationFailed { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_ci_remediation_failed(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "mark_ci_remediation_failed: attempt flipped to failed",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationFailed {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(&sink, &request_id, FrontendEvent::CiRemediationMarkedFailed { attempt });
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown or already terminal",),
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

pub(super) async fn handle_mark_ci_remediation_retriggered(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationRetriggered { attempt_id, new_id } = req else {
        unreachable!()
    };
    {
        // The retrigger marker is the worker's "this is flaky/infra, I
        // re-ran the failing job, there is nothing to push" verdict. The
        // engine flips the attempt to the terminal `retriggered` status and
        // stamps the `ci_flaky_retriggered` signal on the parent. That
        // signal (a) surfaces a flake tag on the task card and (b) tells the
        // completion path to park the worker — awaiting the CI retry / a
        // human decision — instead of re-probing it for a diff that will
        // never exist (the stuck-loop bug). The merge-poller still observes
        // the re-run's outcome on its next sweep and clears the signal when
        // CI goes green.
        match work_db.mark_ci_remediation_retriggered(&attempt_id) {
            Ok(Some(attempt)) => {
                tracing::info!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    new_id = %new_id,
                    "mark_ci_remediation_retriggered: flaky/infra verdict recorded; parent parked awaiting CI retry",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationFlakyRetriggered {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            new_run_id: new_id.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationRetriggered { attempt, new_id },
                );
            }
            // Already terminal (idempotent re-marker) or unknown id.
            // Distinguish the two so the worker's receipt is honest: echo
            // the existing row on a duplicate, error on a forged id.
            Ok(None) => match work_db.get_ci_remediation(&attempt_id) {
                Ok(Some(attempt)) => {
                    tracing::info!(
                        attempt_id = %attempt.id,
                        status = %attempt.status,
                        new_id = %new_id,
                        "mark_ci_remediation_retriggered: attempt already terminal; echoing receipt",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationRetriggered { attempt, new_id },
                    );
                }
                Ok(None) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("ci_remediation attempt {attempt_id:?} is unknown"),
                    },
                ),
                Err(err) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                ),
            },
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

pub(super) async fn handle_mark_ci_remediation_succeeded_via_rebase(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationSucceededViaRebase { attempt_id } = req else {
        unreachable!()
    };
    {
        // Snapshot the pre-update row so we can report
        // `budget_refunded` accurately (only fix-kind attempts
        // with `consumes_budget = 1` get a counter decrement).
        let pre = work_db.get_ci_remediation(&attempt_id).ok().flatten();
        match work_db.mark_ci_remediation_succeeded_via_rebase(&attempt_id) {
            Ok(Some(attempt)) => {
                let budget_refunded = pre.as_ref().map(|p| p.consumes_budget != 0).unwrap_or(false);
                tracing::info!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    budget_refunded,
                    "mark_ci_remediation_succeeded_via_rebase: rebase-only success recorded",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationSucceeded {
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
                    FrontendEvent::CiRemediationSucceededViaRebase {
                        attempt,
                        budget_refunded,
                    },
                );
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown or already terminal",),
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

pub(super) async fn handle_list_ci_remediations(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListCiRemediations {
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    {
        // Read-only listing surface for `boss engine ci list`
        // (design Phase 11 #35). Mirror of
        // `ListConflictResolutions`.
        match work_db.list_ci_remediations(product_id.as_deref(), &status, work_item_id.as_deref(), limit) {
            Ok(attempts) => send_response(&sink, &request_id, FrontendEvent::CiRemediationsList { attempts }),
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

pub(super) async fn handle_get_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCiRemediation { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.get_ci_remediation(&attempt_id) {
            Ok(Some(attempt)) => send_response(&sink, &request_id, FrontendEvent::CiRemediation { attempt }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown",),
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

pub(super) async fn handle_retry_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RetryCiRemediation { selector } = req else {
        unreachable!()
    };
    {
        // The CLI accepts either a `ci_remediations` attempt id
        // or a work-item id (design Q11 "When invoked on an
        // attempt id, the engine resolves the attempt to its
        // work_item_id and acts on the parent."). Resolve the
        // selector before invoking the engine path so the
        // error messages stay grounded in what the caller
        // typed.
        let resolved: Result<Option<String>, anyhow::Error> = if selector.starts_with("cir_") {
            work_db
                .get_ci_remediation(&selector)
                .map(|opt| opt.map(|a| a.work_item_id))
        } else {
            Ok(Some(selector.clone()))
        };
        match resolved {
            Ok(Some(work_item_id)) => match work_db.retry_ci_remediation_for_work_item(&work_item_id) {
                Ok(Some((budget, was_exhausted))) => {
                    tracing::warn!(
                        %work_item_id,
                        was_exhausted,
                        "retry_ci_remediation: budget reset, parent unblocked={was_exhausted}",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationRetryDone {
                            work_item_id,
                            budget,
                            was_exhausted,
                        },
                    );
                }
                Ok(None) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("work item {work_item_id:?} is unknown",),
                    },
                ),
                Err(err) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                ),
            },
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {selector:?} is unknown",),
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

pub(super) async fn handle_abandon_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AbandonCiRemediation { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_ci_remediation_abandoned(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "abandon_ci_remediation: attempt flipped to abandoned",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationAbandoned {
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
                    FrontendEvent::CiRemediationMarkedAbandoned { attempt },
                );
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown or already terminal",),
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

pub(super) async fn handle_get_ci_budget(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCiBudget { work_item_id } = req else {
        unreachable!()
    };
    {
        match work_db.ci_budget_snapshot(&work_item_id) {
            Ok(Some(budget)) => send_response(&sink, &request_id, FrontendEvent::CiBudget { budget }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("work item {work_item_id:?} is unknown"),
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

pub(super) async fn handle_set_ci_budget(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetCiBudget { work_item_id, budget } = req else {
        unreachable!()
    };
    {
        match work_db.set_ci_attempt_budget(&work_item_id, budget) {
            Ok(Some(snapshot)) => {
                send_response(&sink, &request_id, FrontendEvent::CiBudgetUpdated { budget: snapshot })
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("work item {work_item_id:?} is unknown"),
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
