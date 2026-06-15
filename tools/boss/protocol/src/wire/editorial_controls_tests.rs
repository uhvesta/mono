use super::*;
use crate::types::{BranchNaming, EditorialRules, RedactionKind, RedactionRule, TemplatePolicy, TrailerPolicy};

#[test]
fn editorial_rules_defaults_deserialize_from_empty_object() {
    let json = "{}";
    let rules: EditorialRules = serde_json::from_str(json).unwrap();
    assert_eq!(rules, EditorialRules::default());
    assert!(rules.instructions.is_none());
    assert!(rules.redactions.is_empty());
    assert_eq!(rules.template_policy, TemplatePolicy::Off);
    assert_eq!(rules.branch_naming, BranchNaming::BossExecPrefix);
    assert_eq!(rules.commit_trailer_policy, TrailerPolicy::Default);
}

#[test]
fn editorial_rules_round_trip() {
    let rules = EditorialRules {
        instructions: Some("Do not mention Boss.".into()),
        redactions: vec![RedactionRule {
            pattern: "exec_[0-9a-f]{16}".into(),
            replacement: "<id>".into(),
            kind: RedactionKind::Rewrite,
        }],
        template_policy: TemplatePolicy::Enforce,
        branch_naming: BranchNaming::CustomPrefix {
            prefix: "bduff/".into(),
        },
        commit_trailer_policy: TrailerPolicy::NoAiTrailer,
    };
    let json = serde_json::to_string(&rules).unwrap();
    let parsed: EditorialRules = serde_json::from_str(&json).unwrap();
    assert_eq!(rules, parsed);
}

#[test]
fn set_product_editorial_rules_request_round_trips() {
    let input = SetProductEditorialRulesInput {
        product_id: "prod_123".into(),
        rules: Some(EditorialRules {
            template_policy: TemplatePolicy::Advise,
            ..Default::default()
        }),
    };
    let req = FrontendRequest::SetProductEditorialRules { input };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("set_product_editorial_rules"), "serialized: {json}");
    assert!(json.contains("prod_123"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::SetProductEditorialRules { input } => {
            assert_eq!(input.product_id, "prod_123");
            let rules = input.rules.unwrap();
            assert_eq!(rules.template_policy, TemplatePolicy::Advise);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn set_product_editorial_rules_clear_round_trips() {
    let input = SetProductEditorialRulesInput {
        product_id: "prod_456".into(),
        rules: None,
    };
    let req = FrontendRequest::SetProductEditorialRules { input };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::SetProductEditorialRules { input } => {
            assert_eq!(input.product_id, "prod_456");
            assert!(input.rules.is_none());
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn list_editorial_actions_request_round_trips() {
    let req = FrontendRequest::ListEditorialActions {
        product_id: "prod_789".into(),
        limit: Some(25),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("list_editorial_actions"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::ListEditorialActions { product_id, limit } => {
            assert_eq!(product_id, "prod_789");
            assert_eq!(limit, Some(25));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn editorial_actions_list_event_round_trips() {
    let action = EditorialAction {
        id: "ea_001".into(),
        product_id: "prod_789".into(),
        execution_id: "exec_abc123".into(),
        pr_url: Some("https://github.com/org/repo/pull/1".into()),
        tool_command: "gh pr create --title foo --body bar".into(),
        action: "redact".into(),
        reason: "exec_ identifier stripped".into(),
        created_at: "2026-05-30T00:00:00Z".into(),
    };
    let event = FrontendEvent::EditorialActionsList {
        product_id: "prod_789".into(),
        actions: vec![action.clone()],
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("editorial_actions_list"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::EditorialActionsList { product_id, actions } => {
            assert_eq!(product_id, "prod_789");
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0], action);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn evaluate_editorial_rules_request_round_trips() {
    let req = FrontendRequest::EvaluateEditorialRules {
        product_id: "prod_abc".into(),
        body: "## Summary\nFixes the thing with exec_18b07a_1b inside.".into(),
        title: Some("Fix the widget".into()),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("evaluate_editorial_rules"), "serialized: {json}");
    let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendRequest::EvaluateEditorialRules {
            product_id,
            body,
            title,
        } => {
            assert_eq!(product_id, "prod_abc");
            assert!(body.contains("exec_18b07a_1b"));
            assert_eq!(title.as_deref(), Some("Fix the widget"));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn evaluate_editorial_rules_result_event_round_trips() {
    let event = FrontendEvent::EditorialRulesEvaluated {
        product_id: "prod_abc".into(),
        decision: "rewrite".into(),
        findings: vec!["exec_ identifier stripped".into()],
        rewritten_body: Some("## Summary\nFixes the thing inside.".into()),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("editorial_rules_evaluated"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::EditorialRulesEvaluated {
            product_id,
            decision,
            findings,
            rewritten_body,
        } => {
            assert_eq!(product_id, "prod_abc");
            assert_eq!(decision, "rewrite");
            assert_eq!(findings.len(), 1);
            assert!(rewritten_body.is_some());
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn branch_naming_custom_prefix_round_trips() {
    let naming = BranchNaming::CustomPrefix {
        prefix: "bduff/".into(),
    };
    let json = serde_json::to_string(&naming).unwrap();
    assert!(json.contains("custom_prefix"), "serialized: {json}");
    assert!(json.contains("bduff/"), "serialized: {json}");
    let parsed: BranchNaming = serde_json::from_str(&json).unwrap();
    assert_eq!(naming, parsed);
}

#[test]
fn branch_naming_default_is_boss_exec_prefix() {
    let naming = BranchNaming::default();
    assert_eq!(naming, BranchNaming::BossExecPrefix);
    let json = serde_json::to_string(&naming).unwrap();
    assert!(json.contains("boss_exec_prefix"), "serialized: {json}");
    let parsed: BranchNaming = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, BranchNaming::BossExecPrefix);
}

#[test]
fn engine_pool_config_round_trips_through_serde() {
    let event = FrontendEvent::EnginePoolConfig {
        worker_slots: 8,
        automation_slots: 3,
        review_slots: 8,
        coordinator_model: "opus".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"engine_pool_config\""), "serialized: {json}");
    assert!(json.contains("\"worker_slots\":8"), "serialized: {json}");
    assert!(json.contains("\"automation_slots\":3"), "serialized: {json}");
    assert!(json.contains("\"review_slots\":8"), "serialized: {json}");
    let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        FrontendEvent::EnginePoolConfig {
            worker_slots,
            automation_slots,
            review_slots,
            coordinator_model: _,
        } => {
            assert_eq!(worker_slots, 8);
            assert_eq!(automation_slots, 3);
            assert_eq!(review_slots, 8);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn product_with_editorial_rules_round_trips() {
    let rules = EditorialRules {
        instructions: Some("No Boss identifiers in PR body.".into()),
        commit_trailer_policy: TrailerPolicy::NoAiTrailer,
        ..Default::default()
    };
    // Verify the rules serialize correctly within a JSON blob and
    // that `editorial_rules: null` from an absent product column
    // deserialises to `None`.
    let json_with = serde_json::json!({ "editorial_rules": rules });
    let rules_back: EditorialRules = serde_json::from_value(json_with["editorial_rules"].clone()).unwrap();
    assert_eq!(rules_back, rules);

    let json_null = serde_json::json!({ "editorial_rules": null });
    let opt: Option<EditorialRules> = serde_json::from_value(json_null["editorial_rules"].clone()).unwrap();
    assert!(opt.is_none());
}
