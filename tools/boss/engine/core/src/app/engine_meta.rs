//! `FrontendRequest` handlers — engine version/health, feature flags, settings, misc.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_workspace_pool_summary(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::WorkspacePoolSummary = req else {
        unreachable!()
    };
    {
        // Read-only view of `cube workspace list` plus engine
        // annotations. The coordinator contract documents this
        // as a bossctl verb, and any user who can run `cube
        // workspace list` directly already has the same view
        // — so an extra subtree gate buys no security and just
        // breaks legitimate calls (the live coordinator
        // session repro: bossctl invoked from a shell that's
        // neither an app nor a Boss descendant fell through
        // AppOrBoss). User tier is the right level.
        if !server_state.authorize_rpc(RpcTier::User, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                "workspace_pool_summary rejected: caller failed user tier",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "workspace_pool_summary failed user-tier check".to_owned(),
                },
            );
            return;
        }
        match server_state.cube_client.list_workspaces().await {
            Ok(rows) => {
                // Annotate each entry with the engine's view: which
                // execution row (if any) currently records this
                // workspace's lease. Drift (cube reports a lease the
                // engine has no execution for) shows as `None`.
                let lease_to_execution = match server_state.work_db.lease_to_execution_map() {
                    Ok(map) => map,
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "workspace_pool_summary: lease lookup failed; emitting cube view only",
                        );
                        std::collections::HashMap::new()
                    }
                };
                let workspaces = rows
                    .into_iter()
                    .map(|w| {
                        let execution_id = w
                            .lease_id
                            .as_ref()
                            .and_then(|lease_id| lease_to_execution.get(lease_id).cloned());
                        crate::protocol::WorkspacePoolEntry {
                            workspace_id: w.workspace_id,
                            workspace_path: w.workspace_path.display().to_string(),
                            state: w.state,
                            lease_id: w.lease_id,
                            holder: w.holder,
                            task: w.task,
                            leased_at_epoch_s: w.leased_at_epoch_s,
                            lease_expires_at_epoch_s: w.lease_expires_at_epoch_s,
                            execution_id,
                        }
                    })
                    .collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkspacePoolSummaryResult { workspaces },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("cube workspace list failed: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_get_engine_version(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch { sink, request_id, .. } = ctx;
    let FrontendRequest::GetEngineVersion = req else {
        unreachable!()
    };
    {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::EngineVersionResult {
                git_sha: crate::build_info::git_sha().to_owned(),
                build_time: crate::build_info::build_time().to_owned(),
                binary_fingerprint: crate::build_info::binary_fingerprint().to_owned(),
            },
        );
    }
}

pub(super) async fn handle_get_engine_health(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetEngineHealth = req else {
        unreachable!()
    };
    {
        let report = build_engine_health_report(&server_state);
        send_response(&sink, &request_id, FrontendEvent::EngineHealthResult { report });
    }
}

pub(super) async fn handle_list_feature_flags(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListFeatureFlags = req else {
        unreachable!()
    };
    {
        let flags = feature_flags_snapshot_to_wire(&server_state);
        send_response(&sink, &request_id, FrontendEvent::FeatureFlagsList { flags });
    }
}

pub(super) async fn handle_set_feature_flag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetFeatureFlag { name, enabled } = req else {
        unreachable!()
    };
    {
        match server_state.feature_flags.set(&name, enabled) {
            Ok(()) => {
                // Warn the operator when a flag is enabled but its
                // backing capability is absent from this build.
                if enabled {
                    if let Some(spec) = crate::feature_flags::REGISTRY.iter().find(|s| s.name == name) {
                        if let Some(cap_id) = spec.capability_id {
                            if !server_state.capability_registry.is_present(cap_id) {
                                tracing::warn!(
                                    flag = %name,
                                    capability = %cap_id,
                                    "feature-flags: flag enabled but its backing capability \
                                     is absent from this build — the flag will have no effect",
                                );
                            }
                        }
                    }
                }
                tracing::info!(
                    flag = %name,
                    enabled,
                    "feature-flags: toggled via macOS debug pane",
                );
                send_response(&sink, &request_id, FrontendEvent::FeatureFlagSet { name, enabled });
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

/// Update the engine's capability registry with the IDs reported by the
/// macOS app and reply with the updated flag list so the debug pane
/// reflects accurate `capability_present` values immediately.
pub(super) async fn handle_register_capabilities(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RegisterCapabilities { capability_ids } = req else {
        unreachable!()
    };
    server_state.capability_registry.replace_all(capability_ids.into_iter());
    let flags = feature_flags_snapshot_to_wire(&server_state);
    send_response(&sink, &request_id, FrontendEvent::FeatureFlagsList { flags });
}

/// Build the wire-protocol flag list from the live store + capability
/// registry. Extracted so both `ListFeatureFlags` and
/// `RegisterCapabilities` share the same mapping code.
fn feature_flags_snapshot_to_wire(server_state: &ServerState) -> Vec<boss_protocol::FeatureFlagSnapshot> {
    server_state
        .feature_flags
        .snapshot_all(Some(&server_state.capability_registry))
        .into_iter()
        .map(|snap| boss_protocol::FeatureFlagSnapshot {
            name: snap.name,
            description: snap.description,
            category: snap.category,
            default_enabled: snap.default_enabled,
            enabled: snap.enabled,
            capability_present: snap.capability_present,
        })
        .collect()
}

pub(super) async fn handle_get_settings(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetSettings = req else {
        unreachable!()
    };
    {
        let settings = server_state
            .settings
            .snapshot_all()
            .into_iter()
            .map(|snap| boss_protocol::SettingSnapshot {
                key: snap.key,
                description: snap.description,
                default_enabled: snap.default_enabled,
                enabled: snap.enabled,
            })
            .collect();
        send_response(&sink, &request_id, FrontendEvent::SettingsList { settings });
    }
}

pub(super) async fn handle_set_setting(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetSetting { key, enabled } = req else {
        unreachable!()
    };
    {
        match server_state.settings.set(&key, enabled) {
            Ok(()) => {
                tracing::info!(
                    %key,
                    enabled,
                    "settings: toggled via macOS Settings window",
                );
                send_response(&sink, &request_id, FrontendEvent::SettingSet { key, enabled });
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

pub(super) async fn handle_kick_pr_reconcilers(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::KickPrReconcilers = req else {
        unreachable!()
    };
    {
        server_state.pr_reconciler_kick.notify_one();
        tracing::debug!("merge poller: activation kick received from app");
        send_response(&sink, &request_id, FrontendEvent::PrReconcilersKicked { kicked: true });
    }
}

pub(super) async fn handle_set_dispatch_paused(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetDispatchPaused { paused } = req else {
        unreachable!()
    };
    {
        let coordinator = &server_state.execution_coordinator;
        let already = coordinator.is_dispatch_paused();
        if already == paused {
            // Idempotent: no-op but still respond with the current state.
            let paused_since_epoch_s = coordinator.dispatch_paused_since_epoch_s();
            tracing::debug!(paused, "set_dispatch_paused: idempotent no-op");
            send_response(
                &sink,
                &request_id,
                FrontendEvent::DispatchStateResult {
                    paused,
                    paused_since_epoch_s,
                },
            );
            return;
        }
        let now_epoch_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        coordinator.set_dispatch_paused(paused, now_epoch_s);
        // Persist the new state to the metadata table so it survives a restart.
        let db_result = if paused {
            work_db
                .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, &now_epoch_s.to_string()))
        } else {
            work_db
                .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "0")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "0"))
        };
        if let Err(err) = db_result {
            tracing::warn!(
                paused,
                ?err,
                "dispatch_pause: failed to persist to state.db — state is \
                 applied in-memory but will revert on engine restart",
            );
        }
        if paused {
            tracing::info!("dispatch: globally paused — no new executions will be dispatched");
        } else {
            // Re-kick the scheduler so anything that queued while paused is
            // drained immediately without waiting for the next external event.
            coordinator.kick();
            tracing::info!("dispatch: resumed — scheduler kicked to drain queued executions");
        }
        let paused_since_epoch_s = coordinator.dispatch_paused_since_epoch_s();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::DispatchStateResult {
                paused,
                paused_since_epoch_s,
            },
        );
        // Broadcast the new health report to all connected app clients so
        // the pause banner updates live without requiring an app restart.
        server_state.broadcast_engine_health().await;
    }
}

pub(super) async fn handle_get_dispatch_state(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDispatchState = req else {
        unreachable!()
    };
    {
        let coordinator = &server_state.execution_coordinator;
        let paused = coordinator.is_dispatch_paused();
        let paused_since_epoch_s = coordinator.dispatch_paused_since_epoch_s();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::DispatchStateResult {
                paused,
                paused_since_epoch_s,
            },
        );
    }
}
