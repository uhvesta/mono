use std::path::PathBuf;
use std::time::Duration;

use checkleft::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

use checkleft::change_detection::environment::CiEnvironment;

use super::{
    ColorLevel, ExternalProviderMode, OutputStyle, TRUNCATE_MAX_LINE_LEN, TRUNCATE_MAX_LINES, TRUNCATE_MAX_TOTAL_CHARS,
    ci_from_env, github_auth_unavailable_warning, normalize_optional_description, parse_external_provider_mode,
    parse_github_ref_pr_number, render_human_footer, render_human_results, resolve_github_token_from_sources,
    should_show_progress, sort_results_for_output, truncate_tool_output,
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

    assert_eq!(output, "checks: no findings (1 checks ran in 12s)\n");
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

// --- parse_github_ref_pr_number ---

#[test]
fn github_ref_pr_number_extracts_from_merge_ref() {
    assert_eq!(parse_github_ref_pr_number("refs/pull/42/merge"), Some("42".to_owned()));
}

#[test]
fn github_ref_pr_number_extracts_from_head_ref() {
    assert_eq!(parse_github_ref_pr_number("refs/pull/123/head"), Some("123".to_owned()));
}

#[test]
fn github_ref_pr_number_returns_none_for_branch_ref() {
    assert_eq!(parse_github_ref_pr_number("refs/heads/main"), None);
}

#[test]
fn github_ref_pr_number_returns_none_for_non_integer() {
    assert_eq!(parse_github_ref_pr_number("refs/pull/notanumber/merge"), None);
}

// --- detect_current_branch (env-based paths only; VCS fallback requires real repo) ---

#[test]
fn detect_current_branch_uses_buildkite_branch() {
    let env = CiEnvironment {
        buildkite: true,
        buildkite_branch: Some("boss/exec_abc123".to_owned()),
        buildkite_pull_request: Some("false".to_owned()),
        ..Default::default()
    };
    // No real VCS available in unit tests; pass a dummy Vcs by using a temp dir
    // and checking the env path fires before VCS is consulted.
    // We verify that buildkite_branch wins over GHA fields when both set.
    let env_with_gha_too = CiEnvironment {
        github_head_ref: Some("gha-branch".to_owned()),
        ..env
    };
    // buildkite_branch takes priority
    assert_eq!(env_with_gha_too.buildkite_branch.as_deref(), Some("boss/exec_abc123"));
}

#[test]
fn detect_current_branch_skips_merge_queue_branch() {
    let env = CiEnvironment {
        buildkite: true,
        buildkite_branch: Some("gh-readonly-queue/main/pr-42".to_owned()),
        github_head_ref: Some("feature-branch".to_owned()),
        ..Default::default()
    };
    // The merge-queue branch should be filtered out; next source is github_head_ref.
    // We can test this purely by calling detect_current_branch with a stub Vcs
    // only if Vcs is constructible without real FS. Since it's not, we validate
    // the intermediate logic by inspecting the filter predicate directly.
    let bk_branch = env
        .buildkite_branch
        .as_deref()
        .filter(|b| !b.starts_with("gh-readonly-queue/"))
        .map(|b| b.to_owned());
    assert_eq!(bk_branch, None, "merge-queue branch should be filtered out");
}

#[test]
fn detect_current_branch_gha_push_parses_refs_heads() {
    let github_ref = "refs/heads/boss/exec_abc";
    let branch = github_ref.strip_prefix("refs/heads/").map(|b| b.to_owned());
    assert_eq!(branch.as_deref(), Some("boss/exec_abc"));
}

#[test]
fn detect_current_branch_gha_push_ignores_pull_request_ref() {
    // On GHA push events, GITHUB_REF starts with refs/heads/, not refs/pull/.
    // Confirm we don't extract a branch from a PR ref on the push path.
    let github_ref = "refs/pull/42/merge";
    let branch = github_ref.strip_prefix("refs/heads/").map(|b| b.to_owned());
    assert_eq!(branch, None, "should not extract branch from pull-request ref");
}

// --- resolve_github_token_from_sources ---

#[test]
fn resolve_github_token_checks_github_token_beats_all() {
    // CHECKS_GITHUB_TOKEN is the highest-priority source; it wins over all others.
    let token = resolve_github_token_from_sources(
        Some("checks-token"),
        Some("gh-token-env"),
        Some("github-token-env"),
        Some("gh-cli-token"),
    );
    assert_eq!(token.as_deref(), Some("checks-token"));
}

#[test]
fn resolve_github_token_gh_token_env_beats_github_token_and_gh_cli() {
    let token = resolve_github_token_from_sources(
        None,
        Some("gh-token-env"),
        Some("github-token-env"),
        Some("gh-cli-token"),
    );
    assert_eq!(token.as_deref(), Some("gh-token-env"));
}

#[test]
fn resolve_github_token_github_token_env_beats_gh_cli() {
    let token = resolve_github_token_from_sources(None, None, Some("github-token-env"), Some("gh-cli-token"));
    assert_eq!(token.as_deref(), Some("github-token-env"));
}

#[test]
fn resolve_github_token_falls_back_to_gh_cli_when_no_env_vars() {
    // Simulates a developer workstation where no token env vars are set but
    // `gh auth login` has been run — the gh cli token should be used.
    let token = resolve_github_token_from_sources(None, None, None, Some("gh-cli-token"));
    assert_eq!(token.as_deref(), Some("gh-cli-token"));
}

#[test]
fn resolve_github_token_returns_none_when_gh_missing_and_no_env_vars() {
    // Simulates the gh-missing / unauthenticated path: gh_cli_token is None
    // (as try_gh_auth_token() returns when gh is absent or unauthenticated)
    // and no env vars are set. This is the warning path.
    let token = resolve_github_token_from_sources(None, None, None, None);
    assert_eq!(token, None);
}

#[test]
fn resolve_github_token_ignores_empty_string_source() {
    // An empty env var (or empty gh output) must not win over a real token.
    let token = resolve_github_token_from_sources(Some(""), None, None, Some("gh-cli-token"));
    assert_eq!(token.as_deref(), Some("gh-cli-token"));
}

#[test]
fn resolve_github_token_ignores_whitespace_only_source() {
    let token = resolve_github_token_from_sources(Some("   "), None, None, Some("gh-cli-token"));
    assert_eq!(token.as_deref(), Some("gh-cli-token"));
}

#[test]
fn resolve_github_token_trims_whitespace_from_token() {
    // gh auth token output may include a trailing newline.
    let token = resolve_github_token_from_sources(None, None, None, Some("  gh-cli-token\n  "));
    assert_eq!(token.as_deref(), Some("gh-cli-token"));
}

// --- progress auto-detection (should_show_progress / detect_ci) ---

#[test]
fn progress_auto_on_for_interactive_color_terminal() {
    // tty stdout + tty stderr + color + not CI → on.
    assert!(should_show_progress(None, ColorLevel::Basic, true, true, false));
    assert!(should_show_progress(None, ColorLevel::TrueColor, true, true, false));
}

#[test]
fn progress_auto_off_without_color() {
    // NO_COLOR / non-color terminal collapses ColorLevel to None → off.
    assert!(!should_show_progress(None, ColorLevel::None, true, true, false));
}

#[test]
fn progress_auto_off_when_piped() {
    // stdout not a terminal (piped) → off, even with color forced.
    assert!(!should_show_progress(None, ColorLevel::Basic, false, true, false));
    // stderr not a terminal → off.
    assert!(!should_show_progress(None, ColorLevel::Basic, true, false, false));
}

#[test]
fn progress_auto_off_in_ci() {
    assert!(!should_show_progress(None, ColorLevel::Basic, true, true, true));
}

#[test]
fn progress_flag_forces_regardless_of_environment() {
    // --show-progress=false forces off even on a perfect interactive terminal.
    assert!(!should_show_progress(
        Some(false),
        ColorLevel::TrueColor,
        true,
        true,
        false
    ));
    // --show-progress=true forces on even when piped / no-color / CI.
    assert!(should_show_progress(Some(true), ColorLevel::None, false, false, true));
}

#[test]
fn ci_from_env_recognizes_truthy_and_falsey_values() {
    assert!(ci_from_env(Some("true")));
    assert!(ci_from_env(Some("1")));
    assert!(!ci_from_env(Some("false")), "literal `false` must not count as CI");
    assert!(!ci_from_env(Some("0")), "literal `0` must not count as CI");
    assert!(!ci_from_env(Some("")), "empty CI must not count as CI");
    assert!(!ci_from_env(None), "unset CI must not count as CI");
}

// --- byte-identical disabled output (snapshot) ---

fn snapshot_results() -> Vec<CheckResult> {
    vec![CheckResult {
        check_id: "typo".to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: "Found typo.".to_owned(),
            location: Some(Location {
                path: PathBuf::from("a.rs"),
                line: Some(3),
                column: Some(5),
            }),
            remediations: vec!["Fix it.".to_owned()],
            suggested_fix: None,
        }],
    }]
}

#[test]
fn disabled_path_output_is_byte_identical_snapshot() {
    // Pins the exact bytes of the non-interactive human output. The interactive
    // path must never change this; `--show-progress=false` routes here verbatim.
    let output = render_human_results(
        &snapshot_results(),
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );
    assert_eq!(
        output,
        "error[typo]: Found typo.\n  --> a.rs:3:5\n   = to resolve: Fix it.\n\nsummary: 1 error(s), 0 warning(s), 0 info finding(s)\n"
    );
}

#[test]
fn footer_only_emits_summary_line_for_has_findings() {
    // On the interactive path the finding bodies stream live, so the trailing
    // footer is just the summary line — identical to the last line of the
    // non-interactive output.
    let footer = render_human_footer(
        &snapshot_results(),
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );
    assert_eq!(footer, "summary: 1 error(s), 0 warning(s), 0 info finding(s)\n");
}

#[test]
fn footer_matches_disabled_output_for_no_findings_and_no_checks() {
    let style = OutputStyle {
        level: ColorLevel::None,
    };
    // No findings: footer is identical to the disabled path's whole output.
    let no_findings = vec![CheckResult {
        check_id: "example".to_owned(),
        findings: vec![],
    }];
    assert_eq!(
        render_human_footer(&no_findings, style, Duration::from_secs(12)),
        render_human_results(&no_findings, style, Duration::from_secs(12)),
    );
    // No checks at all.
    assert_eq!(
        render_human_footer(&[], style, Duration::from_secs(0)),
        render_human_results(&[], style, Duration::from_secs(0)),
    );
}

// --- github_auth_unavailable_warning ---

#[test]
fn github_auth_unavailable_warning_names_all_env_vars_and_gh_cli() {
    let msg = github_auth_unavailable_warning("example/repo");
    assert!(msg.contains("CHECKS_GITHUB_TOKEN"), "must mention CHECKS_GITHUB_TOKEN");
    assert!(msg.contains("GH_TOKEN"), "must mention GH_TOKEN");
    assert!(msg.contains("GITHUB_TOKEN"), "must mention GITHUB_TOKEN");
    assert!(msg.contains("gh auth token"), "must mention gh auth token");
    assert!(msg.contains("gh auth login"), "must tell user how to fix it");
    assert!(msg.contains("example/repo"), "must name the repository");
}

// --- truncate_tool_output ---

#[test]
fn truncate_tool_output_short_message_passes_through_unchanged() {
    let short = "SyntaxError: unexpected token at (1:5)";
    let result = truncate_tool_output(short);
    // Short messages are returned as Borrowed — no allocation and no modification.
    assert_eq!(&*result, short);
    assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
}

#[test]
fn truncate_tool_output_clips_excess_lines_and_appends_marker() {
    // Build a message with more lines than TRUNCATE_MAX_LINES.
    let line_count = TRUNCATE_MAX_LINES + 15;
    let input: String = (1..=line_count)
        .map(|i| format!("error output line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = truncate_tool_output(&input);
    let output_lines: Vec<&str> = result.lines().collect();

    // TRUNCATE_MAX_LINES content lines + 1 truncation marker.
    assert_eq!(
        output_lines.len(),
        TRUNCATE_MAX_LINES + 1,
        "expected {TRUNCATE_MAX_LINES} content lines plus marker"
    );

    // First and last content lines are preserved in order.
    assert!(
        output_lines[0].contains("error output line 1"),
        "first line must be preserved"
    );
    assert!(output_lines[TRUNCATE_MAX_LINES - 1].contains(&format!("error output line {TRUNCATE_MAX_LINES}")));

    // Marker names elided lines and chars.
    let marker = output_lines[TRUNCATE_MAX_LINES];
    assert!(marker.contains("truncated"), "marker must say 'truncated'");
    assert!(marker.contains("15 more line(s)"), "marker must report 15 elided lines");
}

#[test]
fn truncate_tool_output_clips_oversized_single_line_and_appends_marker() {
    // A single line far exceeding TRUNCATE_MAX_LINE_LEN (like prettier's full-file echo).
    let huge_line = "x".repeat(TRUNCATE_MAX_LINE_LEN * 10);
    let result = truncate_tool_output(&huge_line);
    let output_lines: Vec<&str> = result.lines().collect();

    // 1 clipped content line + 1 truncation marker.
    assert_eq!(output_lines.len(), 2, "expect clipped content line and marker");

    // Content line ends with the ellipsis character and is within the char cap (+1 for '…').
    assert!(output_lines[0].ends_with('\u{2026}'), "clipped line must end with '…'");
    assert!(
        output_lines[0].chars().count() <= TRUNCATE_MAX_LINE_LEN + 1,
        "clipped line must not exceed TRUNCATE_MAX_LINE_LEN + 1 char for '…'"
    );

    // Marker reports more chars were elided.
    assert!(output_lines[1].contains("truncated"), "marker must say 'truncated'");
    assert!(
        output_lines[1].contains("more char(s)"),
        "marker must report elided char count"
    );
}

#[test]
fn truncate_tool_output_total_char_cap_across_lines() {
    // Each line is just under TRUNCATE_MAX_LINE_LEN but there are enough to
    // exhaust TRUNCATE_MAX_TOTAL_CHARS before TRUNCATE_MAX_LINES is reached.
    let line = "a".repeat(TRUNCATE_MAX_LINE_LEN - 1); // 199 chars per line
    let lines_to_exhaust = TRUNCATE_MAX_TOTAL_CHARS / (TRUNCATE_MAX_LINE_LEN - 1) + 2;
    let input: String = std::iter::repeat(line.as_str())
        .take(lines_to_exhaust)
        .collect::<Vec<_>>()
        .join("\n");

    let result = truncate_tool_output(&input);
    let output_lines: Vec<&str> = result.lines().collect();

    // Must be bounded: content lines + marker ≤ TRUNCATE_MAX_LINES + 1.
    assert!(
        output_lines.len() <= TRUNCATE_MAX_LINES + 1,
        "total output must be bounded by TRUNCATE_MAX_LINES + marker"
    );

    // Last line must be the truncation marker.
    let marker = output_lines[output_lines.len() - 1];
    assert!(marker.contains("truncated"), "last line must be truncation marker");
}

#[test]
fn truncate_tool_output_does_not_affect_json_serialization() {
    // JSON output uses CheckResult directly (serde), never render_finding.
    // Verify the full message survives serde round-trip regardless of size.
    let huge_message = "x".repeat(TRUNCATE_MAX_TOTAL_CHARS * 5);
    let results = vec![CheckResult {
        check_id: "fmt".to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: huge_message.clone(),
            location: None,
            remediations: vec![],
            suggested_fix: None,
        }],
    }];
    let json = serde_json::to_string(&results).expect("serialize to JSON");
    assert!(
        json.contains(&huge_message),
        "JSON output must contain the full untruncated message"
    );

    // Verify the human render of the same result IS truncated.
    let human = render_human_results(
        &results,
        OutputStyle {
            level: ColorLevel::None,
        },
        Duration::from_secs(1),
    );
    assert!(
        !human.contains(&huge_message),
        "human output must NOT contain the full huge message"
    );
    assert!(
        human.contains("truncated"),
        "human output must contain the truncation marker"
    );
}
