//! GitHub Contents API helper: fetch a file's raw bytes at a specific ref.
//!
//! Uses `gh api` rather than a direct HTTP call so that credentials are
//! handled by the `gh` CLI installation (same pattern as the rest of Boss).

use std::process::Stdio;

use tokio::process::Command;

/// Fetch the raw content of `path` from `owner/repo` at `ref_name` using
/// `gh api`.
///
/// Returns `Ok(Some(content))` on success, `Ok(None)` when the file does not
/// exist at that ref (HTTP 404 — the common "no file at this branch" case),
/// and `Err` only on a real transport or tool failure.
///
/// `--method GET` is required so `-f ref=` lands in the query string (gh
/// otherwise switches to POST once a field is added), which also makes gh
/// URL-encode slashed branch / ref names like `boss/exec_*` correctly.
pub async fn fetch_repo_file(
    owner: &str,
    repo: &str,
    path: &str,
    ref_name: &str,
) -> anyhow::Result<Option<String>> {
    let endpoint = format!("repos/{owner}/{repo}/contents/{path}");
    let output = Command::new("gh")
        .args([
            "api",
            &endpoint,
            "--method",
            "GET",
            "-f",
            &format!("ref={ref_name}"),
            "-H",
            "Accept: application/vnd.github.raw",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Not Found") || stderr.contains("404") {
        return Ok(None);
    }
    anyhow::bail!(
        "`gh api {endpoint}` failed (exit {:?}): {}",
        output.status.code(),
        stderr.trim()
    )
}
