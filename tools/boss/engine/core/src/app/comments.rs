//! `FrontendRequest` handlers — markdown-viewer comments and magic-wand dispatch.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_comments_create(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsCreate { input } = req else {
        unreachable!()
    };
    {
        let artifact_kind = input.artifact_kind.clone();
        let artifact_id = input.artifact_id.clone();
        match work_db.create_comment(input) {
            Ok(comment) => {
                let revision = publish_comment_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    &artifact_kind,
                    &artifact_id,
                    "comment_created",
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::CommentResult { comment },
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

pub(super) async fn handle_comments_list(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsList {
        artifact_kind,
        artifact_id,
        include_resolved,
    } = req
    else {
        unreachable!()
    };
    match work_db.list_comments(&artifact_kind, &artifact_id, include_resolved) {
        Ok(comments) => send_response_with_revision(
            &sink,
            &request_id,
            server_state.current_work_revision(),
            FrontendEvent::CommentsList {
                artifact_kind,
                artifact_id,
                comments,
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

pub(super) async fn handle_comments_resolve(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsResolve {
        artifact_kind,
        artifact_id,
        plain_text,
        plain_text_projection_version,
    } = req
    else {
        unreachable!()
    };
    {
        let config = crate::comments_anchor::CommentFuzzyConfig::from_env();
        match work_db.resolve_comments(
            &artifact_kind,
            &artifact_id,
            &plain_text,
            plain_text_projection_version,
            &config,
        ) {
            Ok(comments) => send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::CommentsResolved {
                    artifact_kind,
                    artifact_id,
                    comments,
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

pub(super) async fn handle_comments_dismiss(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsDismiss { comment_id, actor } = req else {
        unreachable!()
    };
    {
        match work_db.dismiss_comment(&comment_id, actor.as_deref()) {
            Ok(comment) => {
                let revision = publish_comment_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    "comment_dismissed",
                )
                .await;
                send_response_with_revision(
                    &sink,
                    &request_id,
                    revision,
                    FrontendEvent::CommentResult { comment },
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

pub(super) async fn handle_comments_set_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsSetStatus {
        comment_id,
        status,
        actor,
    } = req
    else {
        unreachable!()
    };
    match work_db.set_comment_status(&comment_id, &status, actor.as_deref()) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_status_changed",
            )
            .await;
            send_response_with_revision(
                &sink,
                &request_id,
                revision,
                FrontendEvent::CommentResult { comment },
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

pub(super) async fn handle_comments_update_anchor(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsUpdateAnchor {
        comment_id,
        anchor,
        new_doc_version,
        plain_text_projection_version,
    } = req
    else {
        unreachable!()
    };
    match work_db.update_comment_anchor(
        &comment_id,
        &anchor,
        &new_doc_version,
        plain_text_projection_version,
    ) {
        Ok(comment) => {
            let revision = publish_comment_invalidation(
                &server_state,
                &session_id,
                &request_id,
                &comment.artifact_kind,
                &comment.artifact_id,
                "comment_anchor_updated",
            )
            .await;
            send_response_with_revision(
                &sink,
                &request_id,
                revision,
                FrontendEvent::CommentResult { comment },
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

pub(super) async fn handle_comments_dispatch_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        peer_pid,
    } = ctx;
    let FrontendRequest::CommentsDispatchMagicWand { comment_id } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                comment_id = %comment_id,
                "comments_dispatch_magic_wand rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "comments_dispatch_magic_wand requires app or Boss authority"
                        .to_owned(),
                },
            );
            return;
        }

        // Resolve the comment to get the doc text and anchor.
        let comment = match work_db.get_comment(&comment_id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown comment: {comment_id}"),
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

        match comment.artifact_kind.as_str() {
            "work_item" => {
                // Phase 3: engine-owned doc → specialised, isolated Claude instance.

                // Fetch the work-item description (the doc text).
                let doc_text = match work_db.get_work_item(&comment.artifact_id) {
                    Ok(item) => {
                        use boss_protocol::WorkItem;
                        match item {
                            WorkItem::Task(t) | WorkItem::Chore(t) => t.description,
                            _ => {
                                send_response(
                                    &sink,
                                    &request_id,
                                    FrontendEvent::WorkError {
                                        message: format!(
                                            "magic-wand dispatch: work item '{}' is not a \
                                                     Task/Chore",
                                            comment.artifact_id
                                        ),
                                    },
                                );
                                return;
                            }
                        }
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

                // Create the dispatch row (status = in_flight).
                let dispatch = match work_db.create_magic_wand_dispatch(
                    &comment_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    &comment.doc_version,
                ) {
                    Ok(d) => d,
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

                // Reply immediately so the macOS app can subscribe to the dispatch topic.
                let dispatch_id = dispatch.id.clone();
                let anchor_exact = comment.anchor.exact.clone();
                let comment_body = comment.body.clone();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MagicWandDispatched {
                        dispatch: dispatch.clone(),
                    },
                );

                // Spawn the async Claude call.
                let work_db2 = work_db.clone();
                let server_state2 = server_state.clone();
                tokio::spawn(async move {
                    let topic = magic_wand_dispatch_topic(&dispatch_id);
                    let result =
                        crate::magic_wand::dispatch(&doc_text, &anchor_exact, &comment_body).await;
                    let final_dispatch = match result {
                        Ok(mw) => {
                            match work_db2.complete_magic_wand_dispatch(
                                &dispatch_id,
                                "returned",
                                Some(&mw.result_md),
                                None,
                                Some(mw.input_tokens),
                                Some(mw.output_tokens),
                                mw.anchor_warning,
                            ) {
                                Ok(d) => d,
                                Err(err) => {
                                    tracing::error!(
                                        dispatch_id = %dispatch_id,
                                        err = %err,
                                        "failed to record magic_wand returned status",
                                    );
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            let (error_msg, error_kind) = err;
                            tracing::warn!(
                                dispatch_id = %dispatch_id,
                                error_kind = %error_kind,
                                error = %error_msg,
                                "magic_wand dispatch failed",
                            );
                            match work_db2.complete_magic_wand_dispatch(
                                &dispatch_id,
                                "failed",
                                None,
                                Some(error_kind),
                                None,
                                None,
                                false,
                            ) {
                                Ok(d) => d,
                                Err(db_err) => {
                                    tracing::error!(
                                        dispatch_id = %dispatch_id,
                                        err = %db_err,
                                        "failed to record magic_wand failed status",
                                    );
                                    return;
                                }
                            }
                        }
                    };
                    let envelope = FrontendEventEnvelope::push(FrontendEvent::MagicWandResult {
                        dispatch: final_dispatch,
                    });
                    server_state2.topic_broker.publish(&topic, envelope).await;
                });
            }

            "pr_doc" => {
                // Phase 4: PR-backed doc → Boss chore worker.
                // Parse the artifact_id: "pr_doc:<repo_remote_url>:<branch>:<path>".
                // repo_remote_url may itself contain ':' (SSH git@ URLs), so we
                // split from the right into exactly 3 parts (path, branch, repo).
                let artifact_id = &comment.artifact_id;
                let suffix = artifact_id.strip_prefix("pr_doc:").unwrap_or("");
                let parts: Vec<&str> = suffix.rsplitn(3, ':').collect();
                if parts.len() != 3 {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "magic-wand: malformed pr_doc artifact_id '{artifact_id}'; \
                                         expected 'pr_doc:<repo>:<branch>:<path>'"
                            ),
                        },
                    );
                    return;
                }
                let (pr_path, pr_branch, pr_repo) = (parts[0], parts[1], parts[2]);

                // Find the product that owns this repo.
                let product_id = match work_db.find_product_id_by_repo_remote_url(pr_repo) {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "magic-wand: no product found for repo '{pr_repo}'; \
                                             cannot spawn chore"
                                ),
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

                // Build the chore title: truncate the anchor quote to 60 chars.
                let short_quote = if comment.anchor.exact.len() > 60 {
                    format!("{}…", &comment.anchor.exact[..60])
                } else {
                    comment.anchor.exact.clone()
                };
                let chore_name = format!("Address comment on `{pr_path}`: `{short_quote}`");

                // Build the chore description (worker reads this as the directive).
                let chore_description = format!(
                    "A reviewer left a comment on this PR's design doc.\n\n\
                             File: {pr_path}\n\
                             Branch: {pr_branch}\n\n\
                             Quoted section:\n\
                             > {anchor}\n\n\
                             Comment:\n\
                             > {body}\n\n\
                             Please update the file accordingly and push to the existing PR \
                             branch. Do not open a new PR; this branch already has one. \
                             Use `git checkout {pr_branch}` (or `jj edit`) to land on the \
                             branch before editing.",
                    anchor = comment.anchor.exact,
                    body = comment.body,
                );

                // Create the chore via the standard path.
                // `repo_remote_url` is inherited from the product
                // (which was resolved by `find_product_id_by_repo_remote_url`),
                // so we don't need to set it again here.
                let chore = match work_db.create_chore(CreateChoreInput {
                    product_id: product_id.clone(),
                    name: chore_name,
                    description: Some(chore_description),
                    autostart: true,
                    priority: None,
                    created_via: Some(format!("comment_dispatch:{comment_id}")),
                    repo_remote_url: None,
                    effort_level: None,
                    model_override: None,
                    force_duplicate: false,
                }) {
                    Ok(c) => c,
                    Err(err) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("magic-wand: failed to create chore: {err}"),
                            },
                        );
                        return;
                    }
                };

                // Create the dispatch row (status = chore_created).
                let dispatch = match work_db.create_pr_backed_magic_wand_dispatch(
                    &comment_id,
                    &comment.artifact_kind,
                    &comment.artifact_id,
                    &comment.doc_version,
                    &chore.id,
                ) {
                    Ok(d) => d,
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

                // Transition the comment to `dispatched`.
                let actor = format!("comment_dispatch:{comment_id}");
                if let Err(err) = work_db.set_comment_status(
                    &comment_id,
                    COMMENT_STATUS_DISPATCHED,
                    Some(actor.as_str()),
                ) {
                    tracing::error!(
                        comment_id = %comment_id,
                        chore_id = %chore.id,
                        err = %err,
                        "magic-wand: failed to transition comment to dispatched",
                    );
                }

                // Publish work invalidation so the kanban sees the new chore.
                publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&product_id)],
                    "comment_dispatch_chore_created",
                    Some(product_id),
                    vec![chore.id.clone()],
                )
                .await;

                tracing::info!(
                    comment_id = %comment_id,
                    chore_id = %chore.id,
                    pr_repo = %pr_repo,
                    pr_branch = %pr_branch,
                    pr_path = %pr_path,
                    "magic-wand: spawned chore for PR-backed doc comment",
                );

                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MagicWandDispatched { dispatch },
                );
            }

            other => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "magic-wand dispatch: unsupported artifact_kind '{other}'"
                        ),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_comments_apply_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsApplyMagicWand {
        dispatch_id,
        current_doc_version,
    } = req
    else {
        unreachable!()
    };
    {
        match work_db.apply_magic_wand_dispatch(&dispatch_id, &current_doc_version, "user") {
            Ok((dispatch, conflict)) => {
                if !conflict {
                    // Publish comment topic invalidation so the sidebar reloads.
                    publish_comment_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        &dispatch.artifact_kind,
                        &dispatch.artifact_id,
                        "magic_wand_applied",
                    )
                    .await;
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MagicWandApplied { dispatch, conflict },
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

pub(super) async fn handle_comments_discard_magic_wand(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CommentsDiscardMagicWand { dispatch_id } = req else {
        unreachable!()
    };
    {
        match work_db.discard_magic_wand_dispatch(&dispatch_id) {
            Ok(dispatch) => send_response(
                &sink,
                &request_id,
                FrontendEvent::MagicWandApplied {
                    dispatch,
                    conflict: false,
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
