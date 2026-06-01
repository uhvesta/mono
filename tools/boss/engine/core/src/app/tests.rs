use super::*;
use super::server::{current_parent_pid, pid_is_alive};
use crate::protocol::TopicEventPayload;

fn topic_envelope(topic: &str, revision: u64) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push_with_revision(
        revision,
        FrontendEvent::TopicEvent {
            topic: topic.to_owned(),
            revision,
            origin_session_id: "test".to_owned(),
            origin_request_id: None,
            event: TopicEventPayload::WorkInvalidated {
                reason: "test".to_owned(),
                product_id: None,
                item_ids: vec![],
            },
        },
    )
}

fn response_envelope(request_id: &str) -> FrontendEventEnvelope {
    FrontendEventEnvelope::response(
        request_id.to_owned(),
        FrontendEvent::ProductsList { products: vec![] },
    )
}

fn topic_of(env: &FrontendEventEnvelope) -> Option<String> {
    topic_event_topic(&env.payload)
}

fn test_server_state() -> Arc<ServerState> {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: temp.path().join("state.db"),
            worker_pool_size: 1,
            automation_pool_size: 1,
        },
        None,
    ));
    // Leak the temp dir for the lifetime of the test process; the
    // ServerState's WorkDb keeps a handle to a path inside it.
    std::mem::forget(temp);
    ServerState::new_arc_with_app_pid(cfg, None, None).unwrap()
}

fn make_session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(shutdown_tx))
}

mod t01;
mod t02;
