//! Wire round-trip tests for the feature-flag, live-metrics, and
//! engine-health request/response variants. Extracted verbatim from
//! `wire.rs` (which had grown past the 3000-line `file/size` limit) into
//! this sibling `#[cfg(test)] mod` file — `super` still resolves to
//! `wire`, so every item these tests reach is unchanged.

use super::*;
use crate::health_wire::{EngineHealthIssue, EngineHealthReport};
use crate::metrics_wire::MetricLiveEntry;

#[test]
fn list_feature_flags_request_round_trips() {
    let original = FrontendRequest::ListFeatureFlags;
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("list_feature_flags"));
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, FrontendRequest::ListFeatureFlags));
}

#[test]
fn set_feature_flag_request_round_trips() {
    let original = FrontendRequest::SetFeatureFlag {
        name: "detect_pr_cold_fallback".into(),
        enabled: false,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("set_feature_flag"));
    assert!(json.contains("detect_pr_cold_fallback"));
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::SetFeatureFlag { name, enabled } => {
            assert_eq!(name, "detect_pr_cold_fallback");
            assert!(!enabled);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn feature_flags_list_event_round_trips() {
    let snap = FeatureFlagSnapshot {
        name: "detect_pr_cold_fallback".into(),
        description: "test description".into(),
        category: "completion".into(),
        default_enabled: true,
        enabled: false,
        capability_present: None,
    };
    let original = FrontendEvent::FeatureFlagsList {
        flags: vec![snap.clone()],
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("feature_flags_list"));
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::FeatureFlagsList { flags } => {
            assert_eq!(flags, vec![snap]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn feature_flags_list_event_round_trips_with_capability_present() {
    let snap = FeatureFlagSnapshot {
        name: "toolbar_search_standard".into(),
        description: "Standard SwiftUI search in toolbar".into(),
        category: "search".into(),
        default_enabled: false,
        enabled: true,
        capability_present: Some(false),
    };
    let json = serde_json::to_string(&snap).unwrap();
    assert!(json.contains("capability_present"));
    let parsed: FeatureFlagSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.capability_present, Some(false));
}

#[test]
fn feature_flag_snapshot_capability_present_defaults_to_none_on_old_payload() {
    // Payloads from older engine builds omit capability_present.
    // The #[serde(default)] annotation must round-trip them as None.
    let json = r#"{"name":"detect_pr_cold_fallback","description":"d","category":"completion","default_enabled":true,"enabled":true}"#;
    let parsed: FeatureFlagSnapshot = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.capability_present, None);
}

#[test]
fn register_capabilities_request_round_trips() {
    let original = FrontendRequest::RegisterCapabilities {
        capability_ids: vec!["toolbar_search_standard".into(), "other_cap".into()],
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("register_capabilities"));
    assert!(json.contains("toolbar_search_standard"));
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::RegisterCapabilities { capability_ids } => {
            assert_eq!(capability_ids, &["toolbar_search_standard", "other_cap"]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn feature_flag_set_event_round_trips() {
    let original = FrontendEvent::FeatureFlagSet {
        name: "detect_pr_cold_fallback".into(),
        enabled: true,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("feature_flag_set"));
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::FeatureFlagSet { name, enabled } => {
            assert_eq!(name, "detect_pr_cold_fallback");
            assert!(enabled);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_list_live_request_round_trips() {
    let original = FrontendRequest::MetricsListLive;
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_list_live"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, FrontendRequest::MetricsListLive));
}

#[test]
fn metrics_list_live_result_event_round_trips() {
    let entries = vec![
        MetricLiveEntry {
            name: "a.counter".into(),
            description: "a counter".into(),
            kind: "counter".into(),
            value: 7,
            timestamp_ms: 1_700_000_000_000,
            stale: false,
        },
        MetricLiveEntry {
            name: "b.gauge".into(),
            description: "a gauge".into(),
            kind: "gauge".into(),
            value: -3,
            timestamp_ms: 1_700_000_001_000,
            stale: true,
        },
    ];
    let original = FrontendEvent::MetricsListLiveResult {
        entries: entries.clone(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_list_live_result"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::MetricsListLiveResult {
            entries: parsed_entries,
        } => {
            assert_eq!(parsed_entries.len(), 2);
            assert_eq!(parsed_entries[0].name, "a.counter");
            assert_eq!(parsed_entries[0].value, 7);
            assert!(!parsed_entries[0].stale);
            assert_eq!(parsed_entries[1].name, "b.gauge");
            assert_eq!(parsed_entries[1].value, -3);
            assert!(parsed_entries[1].stale);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_show_live_request_round_trips() {
    let original = FrontendRequest::MetricsShowLive {
        name: "pr_url_capture.primary_path.hit".into(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_show_live"), "serialized: {json}");
    assert!(json.contains("pr_url_capture.primary_path.hit"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::MetricsShowLive { name } => {
            assert_eq!(name, "pr_url_capture.primary_path.hit");
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_reset_request_round_trips_with_name() {
    let original = FrontendRequest::MetricsReset {
        name: Some("pr_url_capture.primary_path.hit".into()),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_reset"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::MetricsReset { name } => {
            assert_eq!(name.as_deref(), Some("pr_url_capture.primary_path.hit"));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_reset_request_round_trips_with_all() {
    let original = FrontendRequest::MetricsReset { name: None };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_reset"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::MetricsReset { name } => assert!(name.is_none()),
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_show_live_result_event_round_trips() {
    let entry = MetricLiveEntry {
        name: "pr_url_capture.primary_path.hit".into(),
        description: "test desc".into(),
        kind: "counter".into(),
        value: 42,
        timestamp_ms: 1_700_000_000_000,
        stale: false,
    };
    let original = FrontendEvent::MetricsShowLiveResult {
        entry: Some(entry.clone()),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_show_live_result"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::MetricsShowLiveResult { entry: Some(e) } => {
            assert_eq!(e, entry);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn metrics_reset_done_event_round_trips() {
    let original = FrontendEvent::MetricsResetDone {
        name: Some("pr_url_capture.primary_path.hit".into()),
        counters_reset: 1,
        gauges_reset: 0,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("metrics_reset_done"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::MetricsResetDone {
            name,
            counters_reset,
            gauges_reset,
        } => {
            assert_eq!(name.as_deref(), Some("pr_url_capture.primary_path.hit"));
            assert_eq!(counters_reset, 1);
            assert_eq!(gauges_reset, 0);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn get_engine_health_request_round_trips() {
    let original = FrontendRequest::GetEngineHealth;
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("get_engine_health"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, FrontendRequest::GetEngineHealth));
}

#[test]
fn engine_health_result_event_round_trips_healthy() {
    let original = FrontendEvent::EngineHealthResult {
        report: EngineHealthReport {
            anthropic_api_key_present: true,
            dispatch_paused: false,
            issues: Vec::new(),
        },
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("engine_health_result"), "serialized: {json}");
    assert!(json.contains("\"anthropic_api_key_present\":true"), "{json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::EngineHealthResult { report } => {
            assert!(report.anthropic_api_key_present);
            assert!(report.issues.is_empty());
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn engine_health_result_event_round_trips_with_issue() {
    // The macOS app's banner / settings warning binds to the
    // structured `kind` and `severity`, not the freeform `title`
    // or `body`. Pin all four so a refactor that drops the
    // wire-level distinction between kinds gets caught here.
    let issue = EngineHealthIssue {
        kind: "missing_anthropic_api_key".into(),
        severity: "warning".into(),
        title: "ANTHROPIC_API_KEY is not set".into(),
        body: "Summarization is disabled. Set ANTHROPIC_API_KEY in \
               the engine's environment and restart Boss to enable \
               live worker summaries."
            .into(),
    };
    let original = FrontendEvent::EngineHealthResult {
        report: EngineHealthReport {
            anthropic_api_key_present: false,
            dispatch_paused: false,
            issues: vec![issue.clone()],
        },
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("\"kind\":\"missing_anthropic_api_key\""), "{json}");
    assert!(json.contains("\"severity\":\"warning\""), "{json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::EngineHealthResult { report } => {
            assert!(!report.anthropic_api_key_present);
            assert_eq!(report.issues, vec![issue]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}
