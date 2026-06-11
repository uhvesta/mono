//! `FrontendRequest` handlers — live metrics inspection and reset.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_metrics_show_live(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MetricsShowLive { name } = req else {
        unreachable!()
    };
    {
        let counter = server_state.metrics.counter_snapshot_one(&name);
        let gauge = server_state.metrics.gauge_snapshot_one(&name);
        let entry = if let Some(snap) = counter {
            Some(boss_protocol::MetricLiveEntry {
                name: snap.name,
                description: snap.description,
                kind: "counter".into(),
                value: snap.value as i64,
                timestamp_ms: snap.updated_at_ms,
                stale: snap.stale,
            })
        } else {
            gauge.map(|snap| boss_protocol::MetricLiveEntry {
                name: snap.name,
                description: snap.description,
                kind: "gauge".into(),
                value: snap.value,
                timestamp_ms: snap.observed_at_ms,
                stale: snap.stale,
            })
        };
        send_response(&sink, &request_id, FrontendEvent::MetricsShowLiveResult { entry });
    }
}

pub(super) async fn handle_metrics_list_live(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MetricsListLive = req else {
        unreachable!()
    };
    {
        let mut entries: Vec<boss_protocol::MetricLiveEntry> = Vec::new();
        for snap in server_state.metrics.counter_snapshots() {
            entries.push(boss_protocol::MetricLiveEntry {
                name: snap.name,
                description: snap.description,
                kind: "counter".into(),
                value: snap.value as i64,
                timestamp_ms: snap.updated_at_ms,
                stale: snap.stale,
            });
        }
        for snap in server_state.metrics.gauge_snapshots() {
            entries.push(boss_protocol::MetricLiveEntry {
                name: snap.name,
                description: snap.description,
                kind: "gauge".into(),
                value: snap.value,
                timestamp_ms: snap.observed_at_ms,
                stale: snap.stale,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        send_response(&sink, &request_id, FrontendEvent::MetricsListLiveResult { entries });
    }
}

pub(super) async fn handle_metrics_reset(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MetricsReset { name } = req else {
        unreachable!()
    };
    {
        let now = crate::metrics::registry::now_ms();
        let (counters_reset, gauges_reset) = match &name {
            Some(n) => {
                let (c, g) = server_state.metrics.reset_one(n);
                if let Err(err) = work_db.metrics_reset_one(n, now) {
                    tracing::warn!(?err, metric = %n, "metrics reset: db update failed");
                }
                (c as u64, g as u64)
            }
            None => {
                let (c, g) = server_state.metrics.reset_all();
                if let Err(err) = work_db.metrics_reset_all(now) {
                    tracing::warn!(?err, "metrics reset --all: db update failed");
                }
                (c, g)
            }
        };
        send_response(
            &sink,
            &request_id,
            FrontendEvent::MetricsResetDone {
                name,
                counters_reset,
                gauges_reset,
            },
        );
    }
}
