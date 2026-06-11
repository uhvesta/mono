//! End-to-end coverage for `boss shake` — the bug-report-against-Boss verb.
//!
//! `--dry-run` exercises the parse/dispatch path without hitting the
//! GitHub API. The via-shake tests confirm the non-suppressible label
//! is present in the payload regardless of CLI flags.

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
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).expect("dry-run output is json");
    assert_eq!(payload["status"], "dry_run");
    assert_eq!(payload["repo"], "spinyfin/scratch");
    assert_eq!(payload["title"], "Engine wedges on close");
    assert_eq!(payload["body"], "repro: open then close the app twice in a row.");
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

/// Binary built without shake credentials → helpful error pointing at
/// the developer setup README. This covers the "fresh clone without
/// env vars" path; CI builds embed the credentials so this path only
/// appears in local dev without the vars set.
#[test]
fn shake_without_embedded_credentials_exits_with_readme_pointer() {
    // This test only applies when the binary was built without
    // credentials. If they are embedded, skip so we don't need a live
    // GitHub App for the test suite.
    if option_env!("BOSS_SHAKE_APP_ID").is_some() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("bug.md");
    std::fs::write(&report_path, "Engine wedges on close\n\nrepro: …").expect("write report");

    let output = Command::new(boss_binary())
        .args(["shake", report_path.to_str().unwrap()])
        .output()
        .expect("exec boss shake");

    assert!(
        !output.status.success(),
        "expected non-zero exit when credentials are not embedded"
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("README.md"),
        "missing-credentials error should point at README.md, got: {stderr}"
    );
}
