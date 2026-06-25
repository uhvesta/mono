use std::io::Write as _;
use std::sync::OnceLock;

use base64::Engine as _;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

/// How many seconds to wait for each SARIF upload request.
const UPLOAD_TIMEOUT_SECS: u64 = 30;
/// Maximum status poll attempts after a successful upload.
const POLL_ATTEMPTS: u32 = 3;
/// Seconds between status poll attempts.
const POLL_DELAY_SECS: u64 = 2;

/// Context required to upload a SARIF document to GitHub code scanning.
pub struct SarifUploadContext<'a> {
    /// `owner/repo` slug, e.g. `spinyfin/mono`.
    pub repository: &'a str,
    /// GitHub token with `security_events` scope.
    pub token: &'a str,
    /// Full SHA of the commit being analyzed.
    pub commit_sha: &'a str,
    /// Full git ref, e.g. `refs/heads/main` or `refs/pull/42/merge`.
    pub git_ref: &'a str,
}

#[derive(Deserialize)]
struct SarifSubmissionResponse {
    id: String,
}

#[derive(Deserialize)]
struct SarifStatusResponse {
    processing_status: String,
    #[serde(default)]
    errors: Option<Vec<String>>,
}

fn gzip_and_base64(data: &[u8]) -> anyhow::Result<String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    let compressed = encoder.finish()?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&compressed))
}

fn ensure_rustls_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Upload a SARIF 2.1.0 document to GitHub code scanning.
///
/// Non-fatal: auth/API failures are logged as warnings and the function returns
/// `false` rather than propagating errors, matching the discipline used by the
/// PR-description and Check Runs backends.
///
/// Returns `true` when the upload was accepted by GitHub (HTTP 202), `false`
/// when it was skipped or failed non-fatally.
pub async fn upload_sarif(sarif: &Value, ctx: &SarifUploadContext<'_>) -> bool {
    let sarif_json = match serde_json::to_string(sarif) {
        Ok(j) => j,
        Err(e) => {
            warn!("checkleft: SARIF upload skipped — could not serialize SARIF: {e}");
            return false;
        }
    };

    let encoded = match gzip_and_base64(sarif_json.as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            warn!("checkleft: SARIF upload skipped — compression failed: {e}");
            return false;
        }
    };

    ensure_rustls_provider();

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("checkleft: SARIF upload skipped — HTTP client error: {e}");
            return false;
        }
    };

    let url = format!("https://api.github.com/repos/{}/code-scanning/sarifs", ctx.repository);
    let body = serde_json::json!({
        "commit_sha": ctx.commit_sha,
        "ref": ctx.git_ref,
        "sarif": encoded,
    });

    info!(
        repository = ctx.repository,
        git_ref = ctx.git_ref,
        commit_sha = ctx.commit_sha,
        "uploading SARIF to GitHub code scanning"
    );

    let response = match client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "checkleft-cli")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(ctx.token)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("checkleft: SARIF upload failed — network error: {e}");
            return false;
        }
    };

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        warn!("checkleft: SARIF upload failed — HTTP {}: {}", status, body_text.trim());
        return false;
    }

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!("checkleft: SARIF upload succeeded but could not read response body: {e}");
            eprintln!("checkleft: SARIF upload succeeded (id unknown)");
            return true;
        }
    };

    let submission: SarifSubmissionResponse = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!("checkleft: SARIF upload succeeded but could not parse response: {e}");
            eprintln!("checkleft: SARIF upload succeeded (id unknown)");
            return true;
        }
    };

    info!(id = %submission.id, "SARIF upload accepted by GitHub code scanning");
    eprintln!(
        "checkleft: SARIF uploaded to GitHub code scanning (id: {})",
        submission.id
    );

    poll_sarif_status(&client, ctx.repository, &submission.id, ctx.token).await;

    true
}

async fn poll_sarif_status(client: &reqwest::Client, repository: &str, id: &str, token: &str) {
    let url = format!("https://api.github.com/repos/{repository}/code-scanning/sarifs/{id}");

    for attempt in 0..POLL_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(POLL_DELAY_SECS)).await;
        }

        let response = match client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "checkleft-cli")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                info!("checkleft: SARIF status poll failed: {e}");
                return;
            }
        };

        if !response.status().is_success() {
            info!("checkleft: SARIF status poll returned HTTP {}", response.status());
            return;
        }

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(_) => return,
        };
        let status_resp: SarifStatusResponse = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(_) => return,
        };

        match status_resp.processing_status.as_str() {
            "complete" => {
                info!("checkleft: SARIF processing complete");
                eprintln!("checkleft: SARIF processing complete");
                return;
            }
            "failed" => {
                let errors = status_resp.errors.as_deref().unwrap_or(&[]).join("; ");
                warn!("checkleft: SARIF processing failed: {errors}");
                eprintln!("checkleft: SARIF processing failed: {errors}");
                return;
            }
            other => {
                info!(
                    "checkleft: SARIF processing status: {other} (attempt {}/{})",
                    attempt + 1,
                    POLL_ATTEMPTS
                );
            }
        }
    }

    info!("checkleft: SARIF status still pending after {POLL_ATTEMPTS} poll attempts; continuing");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_and_base64_produces_nonempty_output() {
        let data = b"{}";
        let result = gzip_and_base64(data).unwrap();
        assert!(!result.is_empty());
        assert!(
            result
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
    }

    #[test]
    fn gzip_and_base64_roundtrip() {
        use std::io::Read as _;
        let input = br#"{"version":"2.1.0","runs":[]}"#;
        let encoded = gzip_and_base64(input).unwrap();
        let compressed = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();
        assert_eq!(output, input);
    }
}
