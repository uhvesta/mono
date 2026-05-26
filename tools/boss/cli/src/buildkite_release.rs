//! Buildkite API helpers for `boss release`.
//!
//! Triggers a new build on the mono pipeline via the BK REST API and returns
//! the URL of the triggered build. Reads the API token from `BK_API_TOKEN`.
//!
//! See `tools/boss/docs/buildkite-release-setup.md` for provisioning steps.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Buildkite REST API base URL. Overridable via `BOSS_BK_API_BASE` for tests.
pub const DEFAULT_API_BASE: &str = "https://api.buildkite.com";

const USER_AGENT: &str = "boss-release-cli";

/// Subset of the create-build response we surface to the caller.
#[derive(Debug, Deserialize)]
pub struct BuildResponse {
    pub web_url: String,
    pub number: u64,
}

/// Request body for `POST /v2/organizations/:org/pipelines/:pipeline/builds`.
#[derive(Debug, Serialize)]
struct CreateBuildBody<'a> {
    branch: &'a str,
    message: &'a str,
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

/// Trigger a new build on `flunge/mono` and return the resulting `BuildResponse`.
pub async fn trigger_release_build(api_base: &str, token: &str) -> Result<BuildResponse> {
    let url = format!(
        "{}/v2/organizations/flunge/pipelines/mono/builds",
        api_base.trim_end_matches('/')
    );

    let body = CreateBuildBody {
        branch: "main",
        message: "Manual release via boss release",
    };

    let client = http_client();
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .header("User-Agent", USER_AGENT)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        bail!("Buildkite API returned {status}: {err_body}");
    }

    resp.json::<BuildResponse>()
        .await
        .context("decode Buildkite build response")
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn triggers_correct_api_endpoint() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v2/organizations/flunge/pipelines/mono/builds"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "web_url": "https://buildkite.com/flunge/mono/builds/42",
                "number": 42
            })))
            .mount(&server)
            .await;

        let result = trigger_release_build(&server.uri(), "test-token").await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        let build = result.unwrap();
        assert_eq!(build.web_url, "https://buildkite.com/flunge/mono/builds/42");
        assert_eq!(build.number, 42);
    }

    #[tokio::test]
    async fn returns_error_on_api_failure() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v2/organizations/flunge/pipelines/mono/builds"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let result = trigger_release_build(&server.uri(), "bad-token").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("401"), "error should mention status: {msg}");
    }
}
