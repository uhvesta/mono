//! End-to-end coverage for `boss shake` — the bug-report-against-Boss verb.
//!
//! `--dry-run` exercises the parse/dispatch path without hitting the
//! GitHub API. The missing-config test exercises the failure mode the
//! user will hit if they invoke `boss shake` before completing the
//! one-time GitHub App setup documented in PR #748 — the error message
//! must point them back at those instructions.

use std::path::PathBuf;
use std::process::Command;

fn boss_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_boss") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    if let Ok(runfiles_dir) = std::env::var("RUNFILES_DIR") {
        let p = PathBuf::from(runfiles_dir).join("_main/tools/boss/cli/boss");
        if p.exists() {
            return p;
        }
    }
    panic!("boss binary not found; compile with cargo test or bazel test");
}

/// `--dry-run --json` round-trips the parsed title/body/repo. This is the
/// minimum check that the subcommand is wired up and that
/// `split_shake_report` is being called on the file contents.
#[test]
fn shake_dry_run_emits_parsed_payload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("bug.md");
    std::fs::write(
        &report_path,
        "# Engine wedges on close\n\nrepro: open then close the app twice in a row.\n",
    )
    .expect("write report");

    let output = Command::new(boss_binary())
        .args([
            "--json",
            "shake",
            report_path.to_str().unwrap(),
            "--repo",
            "spinyfin/scratch",
            "--label",
            "bug",
            "--dry-run",
        ])
        .output()
        .expect("exec boss shake");

    assert!(
        output.status.success(),
        "boss shake --dry-run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("dry-run output is json");
    assert_eq!(payload["status"], "dry_run");
    assert_eq!(payload["repo"], "spinyfin/scratch");
    assert_eq!(payload["title"], "Engine wedges on close");
    assert_eq!(
        payload["body"],
        "repro: open then close the app twice in a row."
    );
    assert_eq!(payload["labels"][0], "bug");
}

/// Empty file → usage error. Catches a class of "agent gave me a blank
/// file and we filed a blank issue" mistakes.
#[test]
fn shake_rejects_empty_report() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("blank.md");
    std::fs::write(&report_path, "").expect("write blank");

    let output = Command::new(boss_binary())
        .args(["shake", report_path.to_str().unwrap(), "--dry-run"])
        .output()
        .expect("exec boss shake");

    assert!(!output.status.success(), "expected non-zero exit for empty report");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("empty") || stderr.contains("blank"),
        "expected usage error mentioning the empty report, got: {stderr}",
    );
}

/// Missing GitHub App config → loud error that points the user at the
/// setup instructions. This is the path real users will hit on their
/// first non-dry-run `boss shake` before they've completed setup.
#[test]
fn shake_without_app_config_points_at_setup_instructions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("bug.md");
    std::fs::write(&report_path, "Engine wedges on close\n\nrepro: …").expect("write report");

    let missing_config = tmp.path().join("does-not-exist.toml");

    let output = Command::new(boss_binary())
        .env("BOSS_GITHUB_APP_CONFIG", &missing_config)
        .args(["shake", report_path.to_str().unwrap()])
        .output()
        .expect("exec boss shake");

    assert!(
        !output.status.success(),
        "expected non-zero exit when config is missing"
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("PR #748"),
        "missing-config error should point at the setup instructions, got: {stderr}",
    );
    assert!(
        stderr.contains("github-app.toml") || stderr.contains("does-not-exist.toml"),
        "missing-config error should name the file it tried, got: {stderr}",
    );
}
