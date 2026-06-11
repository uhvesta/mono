use std::path::PathBuf;
use std::time::Duration;

use checkleft::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

use super::{
    ColorLevel, ExternalProviderMode, OutputStyle, normalize_optional_description, parse_external_provider_mode,
    render_human_results, sort_results_for_output,
};

#[test]
fn human_output_includes_line_and_column() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo `teh`; use `the` instead.".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("docs/CHECKS.toml"),
                    line: Some(12),
                    column: Some(5),
                }),
                remediations: vec!["Replace typo.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(12),
    );

    assert!(output.contains("error[typo]: Found typo `teh`; use `the` instead."));
    assert!(output.contains("  --> docs/CHECKS.toml:12:5"));
    assert!(output.contains("   = to resolve: Replace typo."));
}

#[test]
fn human_output_omits_ansi_when_color_is_disabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "file-size".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "File exceeds configured line count.".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("backend/src/lib.rs"),
                    line: Some(200),
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: Some(SuggestedFix {
                    description: "Split file by module.".to_owned(),
                    edits: vec![FileEdit {
                        path: PathBuf::from("backend/src/lib.rs"),
                        old_text: "old".to_owned(),
                        new_text: "new".to_owned(),
                    }],
                }),
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(12),
    );

    assert!(!output.contains("\u{1b}["));
    assert!(output.contains("  --> backend/src/lib.rs:200"));
    assert!(output.contains("   = fix: Split file by module. (1 edit)"));
}

#[test]
fn human_output_message_is_bold_when_color_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo.".to_owned(),
                location: None,
                remediations: vec!["Fix it.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::Basic,
        },
        Duration::from_secs(1),
    );

    // Message text is wrapped in bold ANSI
    assert!(output.contains("\u{1b}[1mFound typo.\u{1b}[0m"));
    // Help body is wrapped in dim ANSI
    assert!(output.contains("\u{1b}[2mFix it.\u{1b}[0m"));
}

#[test]
fn human_output_help_body_uses_256_gray_when_color256_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo.".to_owned(),
                location: None,
                remediations: vec!["Fix it.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::Color256,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("\u{1b}[38;5;244mFix it.\u{1b}[0m"));
}

#[test]
fn human_output_help_body_uses_truecolor_gray_when_truecolor_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo.".to_owned(),
                location: None,
                remediations: vec!["Fix it.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::TrueColor,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("\u{1b}[38;2;150;150;150mFix it.\u{1b}[0m"));
}

#[test]
fn human_output_multi_line_remediation_renders_as_bullets() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "check-id".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "something is wrong".to_owned(),
                location: None,
                remediations: vec![
                    "Do option A.".to_owned(),
                    "Do option B.".to_owned(),
                    "Do option C.".to_owned(),
                ],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("   = to resolve:"));
    assert!(output.contains("   - Do option A."));
    assert!(output.contains("   - Do option B."));
    assert!(output.contains("   - Do option C."));
    assert!(!output.contains("   = help:"));
}

#[test]
fn human_output_multi_line_remediation_uses_circle_bullet_when_color_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "check-id".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "something is wrong".to_owned(),
                location: None,
                remediations: vec!["Do option A.".to_owned(), "Do option B.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::Basic,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("   ○ "));
    assert!(!output.contains("   - "));
}

#[test]
fn finding_with_multiple_remediations_renders_as_bullet_list() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "check-id".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "something is wrong".to_owned(),
                location: None,
                remediations: vec!["Do option A.".to_owned(), "Do option B.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("   = to resolve:"));
    assert!(output.contains("   - Do option A."));
    assert!(output.contains("   - Do option B."));
}

#[test]
fn finding_with_multiple_remediations_uses_circle_bullet_when_color_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "check-id".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "something is wrong".to_owned(),
                location: None,
                remediations: vec!["Do option A.".to_owned(), "Do option B.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::Basic,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("   ○ "));
    assert!(!output.contains("   - "));
}

#[test]
fn finding_with_single_remediation_renders_inline() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "check-id".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "something is wrong".to_owned(),
                location: None,
                remediations: vec!["Fix the issue.".to_owned()],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("   = to resolve: Fix the issue."));
    assert!(!output.contains("   - "));
    assert!(!output.contains("   ○ "));
}

#[test]
fn human_output_check_id_is_gray_when_color_enabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "no-debug-logging".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found debug log.".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::Basic,
        },
        Duration::from_secs(1),
    );

    // Severity keyword is bold-red, check id is dimmed
    assert!(output.contains("\u{1b}[1;31merror\u{1b}[0m[\u{1b}[2mno-debug-logging\u{1b}[0m]"));
}

#[test]
fn human_output_check_id_is_plain_when_color_disabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "no-debug-logging".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found debug log.".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );

    assert!(output.contains("error[no-debug-logging]:"));
    assert!(!output.contains("\u{1b}["));
}

#[test]
fn human_output_no_findings_includes_elapsed_time() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "example".to_owned(),
            findings: vec![],
        }],
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(12),
    );

    assert_eq!(output, "checks: no findings (1 checks run in 12s)\n");
}

#[test]
fn output_sorting_prioritizes_error_checks_before_warning_checks() {
    let mut results = vec![
        CheckResult {
            check_id: "alpha-warning".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "warning finding".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        },
        CheckResult {
            check_id: "zeta-error".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "error finding".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        },
    ];

    sort_results_for_output(&mut results);

    assert_eq!(results[0].check_id, "zeta-error");
    assert_eq!(results[1].check_id, "alpha-warning");
}

#[test]
fn output_sorting_orders_findings_within_each_check_by_severity() {
    let mut results = vec![CheckResult {
        check_id: "mixed".to_owned(),
        findings: vec![
            Finding {
                severity: Severity::Warning,
                message: "warning finding".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Info,
                message: "info finding".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Error,
                message: "error finding".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            },
        ],
    }];

    sort_results_for_output(&mut results);

    let severities: Vec<_> = results[0].findings.iter().map(|finding| finding.severity).collect();
    assert_eq!(severities, vec![Severity::Error, Severity::Warning, Severity::Info]);
}

#[test]
fn normalize_optional_description_trims_and_filters_empty_values() {
    assert_eq!(normalize_optional_description(None), None);
    assert_eq!(normalize_optional_description(Some("".to_owned())), None);
    assert_eq!(
        normalize_optional_description(Some("  235  ".to_owned())),
        Some("235".to_owned())
    );
}

#[test]
fn parse_external_provider_mode_defaults_to_auto() {
    let parsed = parse_external_provider_mode(None).expect("parse mode");
    assert_eq!(parsed, ExternalProviderMode::Auto);
}

#[test]
fn parse_external_provider_mode_accepts_supported_values() {
    assert_eq!(
        parse_external_provider_mode(Some("file-only".to_owned())).expect("parse"),
        ExternalProviderMode::FileOnly
    );
    assert_eq!(
        parse_external_provider_mode(Some("generated-only".to_owned())).expect("parse"),
        ExternalProviderMode::GeneratedOnly
    );
    assert_eq!(
        parse_external_provider_mode(Some("off".to_owned())).expect("parse"),
        ExternalProviderMode::Off
    );
}

#[test]
fn parse_external_provider_mode_rejects_invalid_values() {
    let error = parse_external_provider_mode(Some("unknown".to_owned())).expect_err("must fail");
    assert!(error.to_string().contains("invalid `CHECKLEFT_EXTERNAL_PROVIDER_MODE`"));
}
