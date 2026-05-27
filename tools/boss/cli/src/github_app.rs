//! GitHub App authentication for `boss shake`.
//!
//! The verb files an issue against `spinyfin/mono` (the upstream Boss
//! repo). Authentication has to work in the user's corporate
//! environment, where `gh` requires a wrapper alias to function, so we
//! cannot shell out to `gh`. Instead we authenticate as a registered
//! GitHub App: sign a JWT with the App's RSA private key, exchange it
//! for a short-lived installation access token, and use that token on
//! the issue-create call.
//!
//! Credentials are embedded at build time from three env vars:
//! `BOSS_SHAKE_APP_ID`, `BOSS_SHAKE_INSTALLATION_ID`, and
//! `BOSS_SHAKE_PRIVATE_KEY_PEM`. See `tools/boss/cli/README.md` for
//! the one-time developer setup instructions.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

/// Default base URL for the GitHub REST API. Overridable via
/// `BOSS_GITHUB_API_BASE` so tests can point at a wiremock instance.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Node ID for spinyfin Project #1 ("Boss").
/// https://github.com/orgs/spinyfin/projects/1
/// Used as the default target project when `boss shake` files an issue.
pub const DEFAULT_PROJECT_NODE_ID: &str = "PVT_kwDOAvvvSM4BX0pJ";

/// Label added to every issue filed via `boss shake`. Unstrippable —
/// present even when the user passes `--label` flags. Primary
/// abuse-mitigation lever: issues can be bulk-filtered or cleaned by
/// querying this label.
const VIA_SHAKE_LABEL: &str = "via-shake";

/// User-Agent on every GitHub API call. GitHub rejects calls without
/// one; a stable string makes our traffic identifiable in audit logs.
const USER_AGENT: &str = "boss-shake";

/// JWT lifetime. GitHub allows up to 10 minutes; 9 gives clock-skew
/// slack on both ends.
const JWT_TTL_SECS: u64 = 9 * 60;

// Credentials embedded at compile time from environment variables.
// Set BOSS_SHAKE_APP_ID, BOSS_SHAKE_INSTALLATION_ID, and
// BOSS_SHAKE_PRIVATE_KEY_PEM before building. See tools/boss/cli/README.md.
const EMBEDDED_APP_ID: Option<&str> = option_env!("BOSS_SHAKE_APP_ID");
const EMBEDDED_INSTALLATION_ID: Option<&str> = option_env!("BOSS_SHAKE_INSTALLATION_ID");
const EMBEDDED_PRIVATE_KEY_PEM: Option<&str> = option_env!("BOSS_SHAKE_PRIVATE_KEY_PEM");

/// App credentials embedded at build time.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub app_id: String,
    pub installation_id: String,
    /// Full PEM contents (RSA private key) embedded at compile time.
    pub private_key_pem: String,
}

/// Load credentials that were embedded at build time. Fails if this
/// binary was built without the three `BOSS_SHAKE_*` env vars set.
/// See tools/boss/cli/README.md for developer setup instructions.
pub fn embedded_config() -> Result<AppConfig> {
    // filter(|s| !s.is_empty()): the rustc_env Make-variable expansion
    // always sets the vars (to "" when no --define override is passed),
    // so option_env! returns Some("") for dev builds. Treat that the same
    // as absent so the sentinel error fires instead of an empty-creds failure.
    let app_id = EMBEDDED_APP_ID.filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!(
            "this build was produced without shake credentials; \
             see tools/boss/cli/README.md for developer setup instructions"
        ))?;
    let installation_id = EMBEDDED_INSTALLATION_ID.filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!(
            "this build was produced without shake credentials; \
             see tools/boss/cli/README.md for developer setup instructions"
        ))?;
    let private_key_pem = EMBEDDED_PRIVATE_KEY_PEM.filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!(
            "this build was produced without shake credentials; \
             see tools/boss/cli/README.md for developer setup instructions"
        ))?;
    Ok(AppConfig {
        app_id: app_id.to_owned(),
        installation_id: installation_id.to_owned(),
        private_key_pem: private_key_pem.to_owned(),
    })
}

/// Result of `POST /app/installations/{id}/access_tokens`.
#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
}

/// Body for `POST /repos/{owner}/{repo}/issues`. Only the fields we use.
#[derive(Debug, Serialize)]
struct CreateIssueBody<'a> {
    title: &'a str,
    body: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    labels: &'a [String],
}

/// Subset of the create-issue response we surface back to the caller.
/// The full response carries dozens of fields; we only need the URL,
/// issue number, and node_id (for project association via GraphQL).
#[derive(Debug, Deserialize)]
pub struct IssueResponse {
    pub html_url: String,
    pub number: u64,
    pub node_id: String,
}

/// Mint a 9-minute JWT signed with the App's private key. GitHub
/// accepts this as proof we control the App; we trade it for an
/// installation token in the next step.
pub fn build_jwt(app_id: &str, private_key_pem: &[u8]) -> Result<String> {
    build_jwt_at(app_id, private_key_pem, current_unix_time()?)
}

/// `build_jwt` with the issued-at time injected so tests can pin it.
fn build_jwt_at(app_id: &str, private_key_pem: &[u8], iat: u64) -> Result<String> {
    let key = EncodingKey::from_rsa_pem(private_key_pem)
        .context("parse GitHub App private key (must be PEM-encoded RSA)")?;

    #[derive(Serialize)]
    struct Claims<'a> {
        iat: u64,
        exp: u64,
        iss: &'a str,
    }

    let claims = Claims {
        iat: iat.saturating_sub(60), // GitHub allows 60s skew; subtract to absorb it.
        exp: iat + JWT_TTL_SECS,
        iss: app_id,
    };
    let header = Header::new(Algorithm::RS256);
    encode(&header, &claims, &key).context("sign JWT for GitHub App")
}

fn current_unix_time() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .context("system clock is before UNIX epoch")
}

/// Process-wide reqwest client. Matches the engine's pane_summary
/// pattern: install the rustls default provider once, then share a
/// pooled client across calls.
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

/// Exchange the App JWT for a short-lived installation access token.
async fn fetch_installation_token(
    api_base: &str,
    installation_id: &str,
    jwt: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let url = format!(
        "{}/app/installations/{}/access_tokens",
        api_base.trim_end_matches('/'),
        installation_id
    );
    let resp = client
        .post(&url)
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("installation token exchange returned {status}: {body}");
    }
    let parsed: InstallationTokenResponse = resp
        .json()
        .await
        .context("decode installation token response")?;
    Ok(parsed.token)
}

/// Build the effective label list: user-supplied labels plus the
/// mandatory `via-shake` label. The `via-shake` label is always
/// present regardless of what the user passes; it is the primary
/// abuse-mitigation lever on the GitHub side.
pub fn build_labels(user_labels: &[String]) -> Vec<String> {
    let mut labels: Vec<String> = user_labels.to_vec();
    if !labels.iter().any(|l| l == VIA_SHAKE_LABEL) {
        labels.push(VIA_SHAKE_LABEL.to_owned());
    }
    labels
}

/// Create an issue against `repo` (must be `owner/name`) using
/// `token` as Bearer credentials.
async fn create_issue(
    api_base: &str,
    repo: &str,
    title: &str,
    body: &str,
    labels: &[String],
    token: &str,
    client: &reqwest::Client,
) -> Result<IssueResponse> {
    let url = format!(
        "{}/repos/{}/issues",
        api_base.trim_end_matches('/'),
        repo
    );
    // Append a hidden attribution comment so the issue source is
    // traceable even if the via-shake label is manually removed.
    let attributed_body = format!("{body}\n\n<!-- via boss shake -->");
    let effective_labels = build_labels(labels);
    let payload = CreateIssueBody {
        title,
        body: &attributed_body,
        labels: &effective_labels,
    };
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("issue create returned {status}: {body}");
    }
    resp.json::<IssueResponse>()
        .await
        .context("decode issue-create response")
}

/// Request body for the GitHub GraphQL endpoint.
#[derive(Debug, Serialize)]
struct GraphqlRequest<'a> {
    query: &'a str,
    variables: serde_json::Value,
}

/// Top-level wrapper around a GraphQL response. GitHub returns HTTP 200
/// even for mutation errors; the real outcome is in `errors`.
#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

/// Add a GitHub issue (identified by its node id) to a GitHub Project V2
/// (identified by its project node id) via the `addProjectV2ItemById`
/// GraphQL mutation.
///
/// Returns an error when:
/// - the HTTP request fails,
/// - GitHub returns a non-2xx status, or
/// - the response body contains a GraphQL `errors` array (e.g. the
///   GitHub App is missing the Projects read/write scope — GitHub
///   surfaces "Resource not accessible by integration" in that case).
pub async fn add_issue_to_project(
    api_base: &str,
    project_node_id: &str,
    issue_node_id: &str,
    token: &str,
    client: &reqwest::Client,
) -> Result<()> {
    let url = format!("{}/graphql", api_base.trim_end_matches('/'));
    let payload = GraphqlRequest {
        query: "mutation AddToProject($projectId: ID!, $contentId: ID!) { \
                  addProjectV2ItemById(input: {projectId: $projectId, contentId: $contentId}) { \
                    item { id } \
                  } \
                }",
        variables: serde_json::json!({
            "projectId": project_node_id,
            "contentId": issue_node_id,
        }),
    };
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "add-to-project returned {status}: {body}\n\
             hint: confirm the GitHub App has Projects read/write permission"
        );
    }

    let parsed: GraphqlResponse = resp
        .json()
        .await
        .context("decode add-to-project GraphQL response")?;

    if let Some(errors) = parsed.errors {
        if !errors.is_empty() {
            let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
            bail!(
                "add-to-project GraphQL mutation failed: {}\n\
                 hint: confirm the GitHub App has Projects read/write permission",
                messages.join("; ")
            );
        }
    }

    Ok(())
}

/// Top-level entry point used by the CLI: use the embedded App config,
/// sign a JWT, swap it for an installation token, then file the issue.
/// The `api_base` is parametrized so tests can point at a wiremock;
/// the CLI caller passes [`DEFAULT_API_BASE`] (or whatever
/// `BOSS_GITHUB_API_BASE` resolves to).
pub async fn file_issue(
    config: &AppConfig,
    api_base: &str,
    repo: &str,
    title: &str,
    body: &str,
    labels: &[String],
) -> Result<IssueResponse> {
    let jwt = build_jwt(&config.app_id, config.private_key_pem.as_bytes())?;
    let client = http_client();
    let token = fetch_installation_token(api_base, &config.installation_id, &jwt, client).await?;
    create_issue(api_base, repo, title, body, labels, &token, client).await
}

/// Top-level entry point for adding an issue to a GitHub Project V2.
/// Signs a JWT, exchanges it for an installation token, and calls
/// [`add_issue_to_project`].
pub async fn add_issue_to_project_with_embedded_token(
    config: &AppConfig,
    api_base: &str,
    project_node_id: &str,
    issue_node_id: &str,
) -> Result<()> {
    let jwt = build_jwt(&config.app_id, config.private_key_pem.as_bytes())?;
    let client = http_client();
    let token = fetch_installation_token(api_base, &config.installation_id, &jwt, client).await?;
    add_issue_to_project(api_base, project_node_id, issue_node_id, &token, client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation, decode};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A throwaway RSA-2048 keypair generated offline (`openssl genrsa
    /// 2048` + `openssl rsa -pubout`). Loaded via `include_str!` from
    /// `tests/fixtures/` so the actual armoring matches what GitHub's
    /// App download UI emits.
    const TEST_RSA_PEM: &str = include_str!("../tests/fixtures/github-app-private-key.pem");
    const TEST_RSA_PUBLIC_PEM: &str = include_str!("../tests/fixtures/github-app-public-key.pem");

    #[test]
    fn build_jwt_produces_decodable_rs256_token() {
        let token = build_jwt_at("42", TEST_RSA_PEM.as_bytes(), 1_700_000_000).unwrap();

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&["42"]);
        validation.validate_exp = false;
        let key = DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_PEM.as_bytes()).unwrap();

        #[derive(Deserialize)]
        struct Claims {
            iat: u64,
            exp: u64,
            iss: String,
        }
        let decoded = decode::<Claims>(&token, &key, &validation).unwrap();
        assert_eq!(decoded.claims.iss, "42");
        // iat is clamped 60s into the past for clock-skew tolerance.
        assert_eq!(decoded.claims.iat, 1_700_000_000 - 60);
        assert_eq!(decoded.claims.exp, 1_700_000_000 + JWT_TTL_SECS);
    }

    #[test]
    fn build_jwt_rejects_garbage_pem() {
        let err = build_jwt("42", b"not a pem").unwrap_err();
        assert!(
            err.to_string().contains("private key"),
            "error should mention private key: {err}"
        );
    }

    #[test]
    fn build_labels_always_includes_via_shake() {
        // With no user labels.
        let labels = build_labels(&[]);
        assert!(labels.contains(&VIA_SHAKE_LABEL.to_owned()));

        // With user labels, via-shake is still present.
        let labels = build_labels(&["bug".to_string(), "feature".to_string()]);
        assert!(labels.contains(&VIA_SHAKE_LABEL.to_owned()));
        assert!(labels.contains(&"bug".to_string()));

        // Passing via-shake explicitly doesn't duplicate it.
        let labels = build_labels(&[VIA_SHAKE_LABEL.to_owned()]);
        assert_eq!(labels.iter().filter(|l| *l == VIA_SHAKE_LABEL).count(), 1);
    }

    fn make_test_config() -> AppConfig {
        AppConfig {
            app_id: "42".into(),
            installation_id: "67890".into(),
            private_key_pem: TEST_RSA_PEM.to_owned(),
        }
    }

    #[tokio::test]
    async fn file_issue_round_trips_against_mock_github() {
        let server = MockServer::start().await;

        // Capture: matches the installation-token endpoint with a
        // Bearer JWT and returns a canned token.
        Mock::given(method("POST"))
            .and(path("/app/installations/67890/access_tokens"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"token": "v1.installation-token"})),
            )
            .mount(&server)
            .await;

        // Capture: matches the issue-create endpoint with the
        // installation token. Returns a canned issue URL + node_id.
        Mock::given(method("POST"))
            .and(path("/repos/spinyfin/mono/issues"))
            .and(header("authorization", "Bearer v1.installation-token"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "html_url": "https://github.com/spinyfin/mono/issues/9999",
                "number": 9999,
                "node_id": "I_kwDOAvvvSM4BX0pJ",
            })))
            .mount(&server)
            .await;

        let cfg = make_test_config();

        let resp = file_issue(
            &cfg,
            &server.uri(),
            "spinyfin/mono",
            "Engine wedges",
            "repro: open then close",
            &["bug".to_string()],
        )
        .await
        .expect("file_issue should succeed against the mock");
        assert_eq!(resp.number, 9999);
        assert_eq!(resp.html_url, "https://github.com/spinyfin/mono/issues/9999");
        assert_eq!(resp.node_id, "I_kwDOAvvvSM4BX0pJ");
    }

    #[tokio::test]
    async fn file_issue_always_sends_via_shake_label() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/app/installations/67890/access_tokens"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"token": "v1.token"})),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/repos/spinyfin/mono/issues"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "html_url": "https://github.com/spinyfin/mono/issues/1",
                "number": 1,
                "node_id": "I_kwDOAvvvSM4BX0pJ",
            })))
            .mount(&server)
            .await;

        let cfg = make_test_config();
        file_issue(
            &cfg,
            &server.uri(),
            "spinyfin/mono",
            "title",
            "body",
            &["bug".to_string()],
        )
        .await
        .expect("file_issue should succeed against the mock");

        // Inspect the captured request body to assert via-shake is present.
        let requests = server.received_requests().await.unwrap();
        let issue_req = requests
            .iter()
            .find(|r| r.url.path().ends_with("/issues"))
            .expect("issue-create request was not captured");
        let body: serde_json::Value =
            serde_json::from_slice(&issue_req.body).expect("issue body is JSON");
        let labels = body["labels"].as_array().expect("labels is an array");
        let label_strs: Vec<&str> = labels
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            label_strs.contains(&"via-shake"),
            "via-shake must be in the labels array; got: {label_strs:?}"
        );
        assert!(
            label_strs.contains(&"bug"),
            "user-supplied label 'bug' must be preserved; got: {label_strs:?}"
        );
    }

    #[tokio::test]
    async fn file_issue_surfaces_github_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/67890/access_tokens"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("{\"message\":\"Bad credentials\"}"),
            )
            .mount(&server)
            .await;

        let cfg = make_test_config();

        let err = file_issue(&cfg, &server.uri(), "spinyfin/mono", "t", "b", &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") && msg.contains("Bad credentials"),
            "error should surface github's 401 body: {msg}"
        );
    }

    #[tokio::test]
    async fn add_issue_to_project_succeeds_on_200_with_no_errors() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "addProjectV2ItemById": {
                        "item": { "id": "PVTI_lADOAvvvSM4BX0pJ" }
                    }
                }
            })))
            .mount(&server)
            .await;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        add_issue_to_project(
            &server.uri(),
            "PVT_kwDOAvvvSM4BX0pJ",
            "I_kwDOAvvvSM4BX0pJ",
            "v1.test-token",
            &client,
        )
        .await
        .expect("add_issue_to_project should succeed");

        // Verify the mutation sent the right variables.
        let requests = server.received_requests().await.unwrap();
        let req = requests
            .iter()
            .find(|r| r.url.path() == "/graphql")
            .expect("graphql request was not captured");
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).expect("request body is JSON");
        assert_eq!(
            body["variables"]["projectId"],
            "PVT_kwDOAvvvSM4BX0pJ",
            "projectId variable must match"
        );
        assert_eq!(
            body["variables"]["contentId"],
            "I_kwDOAvvvSM4BX0pJ",
            "contentId variable must match"
        );
    }

    #[tokio::test]
    async fn add_issue_to_project_surfaces_graphql_errors() {
        let server = MockServer::start().await;

        // GitHub returns HTTP 200 even when the mutation fails.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errors": [
                    { "message": "Resource not accessible by integration" }
                ]
            })))
            .mount(&server)
            .await;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let err = add_issue_to_project(
            &server.uri(),
            "PVT_kwDOAvvvSM4BX0pJ",
            "I_kwDOAvvvSM4BX0pJ",
            "v1.test-token",
            &client,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("Resource not accessible by integration"),
            "error should include the GraphQL error message: {msg}"
        );
        assert!(
            msg.contains("Projects read/write permission"),
            "error should hint about missing permission: {msg}"
        );
    }

    #[tokio::test]
    async fn add_issue_to_project_surfaces_http_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("{\"message\":\"Forbidden\"}"),
            )
            .mount(&server)
            .await;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let err = add_issue_to_project(
            &server.uri(),
            "PVT_kwDOAvvvSM4BX0pJ",
            "I_kwDOAvvvSM4BX0pJ",
            "v1.test-token",
            &client,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("403"),
            "error should surface HTTP 403: {msg}"
        );
        assert!(
            msg.contains("Projects read/write permission"),
            "error should hint about missing permission: {msg}"
        );
    }
}
