//! Host-registry RPC handlers (AddHost, GetHost, ListHosts, SetHostEnabled,
//! RemoveHost, AddHostTag, RemoveHostTag). Thin wrappers over `WorkDb`'s
//! host CRUD methods; the add path eagerly pushes the remote wrapper
//! (same as `bossctl hosts add`) so the engine is the single owner of
//! the host lifecycle. Dispatched from `app.rs`.

use super::*;
use crate::host_registry::{Host, HostCapability};
use crate::protocol::{HostCapabilitySnapshot, HostSnapshot};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn to_host_snapshot(host: Host, caps: Vec<HostCapability>) -> HostSnapshot {
    HostSnapshot {
        id: host.id,
        ssh_target: host.ssh_target,
        pool_size: host.pool_size,
        enabled: host.enabled,
        last_seen_at: host.last_seen_at,
        last_error_text: host.last_error_text,
        created_at: host.created_at,
        capabilities: caps
            .into_iter()
            .map(|c| HostCapabilitySnapshot {
                capability: c.capability,
                source: c.source,
            })
            .collect(),
    }
}

fn fetch_snapshot(work_db: &crate::work::WorkDb, id: &str) -> anyhow::Result<HostSnapshot> {
    let host = work_db
        .get_host(id)?
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found", id))?;
    let caps = work_db.list_host_capabilities(id)?;
    Ok(to_host_snapshot(host, caps))
}

fn send_error_msg(sink: &super::SessionSink, request_id: &str, msg: impl Into<String>) {
    send_response(sink, request_id, FrontendEvent::Error { message: msg.into() });
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(super) async fn handle_list_hosts(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListHosts = req else {
        unreachable!()
    };
    let result = (|| -> anyhow::Result<Vec<HostSnapshot>> {
        let hosts = work_db.list_hosts()?;
        hosts
            .into_iter()
            .map(|h| {
                let caps = work_db.list_host_capabilities(&h.id)?;
                Ok(to_host_snapshot(h, caps))
            })
            .collect()
    })();
    match result {
        Ok(hosts) => {
            send_response(&sink, &request_id, FrontendEvent::HostsList { hosts });
        }
        Err(err) => {
            tracing::warn!(?err, "list_hosts failed");
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_get_host(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetHost { id } = req else {
        unreachable!()
    };
    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostResult { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_add_host(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddHost {
        id,
        ssh_target,
        pool_size,
        tags,
    } = req
    else {
        unreachable!()
    };

    // Insert the host row.
    if let Err(err) = work_db.add_host(&id, &ssh_target, pool_size, &tags) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }

    // Eagerly push the remote wrapper (same path as `bossctl hosts add`).
    eager_push_wrapper_rpc(&work_db, &id, &ssh_target).await;

    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostResult { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_set_host_enabled(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetHostEnabled { id, enabled } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.set_host_enabled(&id, enabled) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_remove_host(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveHost { id } = req else {
        unreachable!()
    };
    match work_db.remove_host(&id) {
        Ok(()) => {
            send_response(&sink, &request_id, FrontendEvent::HostRemoved { id });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_add_host_tag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddHostTag { host_id, tag } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.add_user_host_capability(&host_id, &tag) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &host_id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_remove_host_tag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveHostTag { host_id, tag } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.remove_user_host_capability(&host_id, &tag) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &host_id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

// ── Eager wrapper push (mirrors bossctl hosts add path) ──────────────────────

/// Push the boss-remote-run wrapper to the remote host. On failure the host is
/// disabled with `last_error_text` set; the caller reads the updated snapshot
/// to surface the result to the UI (disabled + error text = add failed).
async fn eager_push_wrapper_rpc(work_db: &crate::work::WorkDb, host_id: &str, ssh_target: &str) {
    use crate::ssh_transport::{SshTransport, default_control_socket_dir};
    use crate::wrapper_distribution::{WrapperPushOutcome, push_wrapper, subclass_label};

    let Some(socket_dir) = default_control_socket_dir() else {
        tracing::warn!(host_id, "eager_push_wrapper: HOME unset; skipping");
        return;
    };
    let transport = SshTransport::new(host_id, ssh_target, &socket_dir);

    if let Err(err) = transport.open_control_master().await {
        let detail = format!("opening ssh control master: {err:#}");
        tracing::warn!(host_id, %detail, "eager_push_wrapper: ssh connection failed");
        let _ = work_db.set_host_enabled(host_id, false);
        let _ = work_db.set_host_last_error(host_id, Some(&detail));
        return;
    }

    match push_wrapper(&transport).await {
        Ok(WrapperPushOutcome::Ok) => {
            let _ = work_db.set_host_last_error(host_id, None);
        }
        Ok(WrapperPushOutcome::Failed(kind, detail)) => {
            let label = subclass_label(&kind);
            let msg = format!("wrapper push failed ({label}): {detail}");
            tracing::warn!(host_id, %msg, "eager_push_wrapper: push failed");
            let _ = work_db.set_host_enabled(host_id, false);
            let _ = work_db.set_host_last_error(host_id, Some(&msg));
        }
        Err(err) => {
            let msg = format!("wrapper push errored: {err:#}");
            tracing::warn!(host_id, %msg, "eager_push_wrapper: error");
            let _ = work_db.set_host_enabled(host_id, false);
            let _ = work_db.set_host_last_error(host_id, Some(&msg));
        }
    }
}
