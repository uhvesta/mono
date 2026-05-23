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
//! Setup is documented in the PR body for #748; in short the user
//! registers a GitHub App, installs it on the target repo, and drops a
//! `github-app.toml` at `~/Library/Application Support/Boss/` pointing
//! at the App's downloaded private key. The verb fails loud with a
//! pointer to those instructions if the config is missing or any field
//! is still a placeholder.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

/// Where the App config lives by default. The user can override with
/// `BOSS_GITHUB_APP_CONFIG` (mainly for tests; an advanced user could
/// also point it at a per-machine alt config).
pub const DEFAULT_CONFIG_REL: &str = "Library/Application Support/Boss/github-app.toml";

/// Default base URL for the GitHub REST API. Overridable via
/// `BOSS_GITHUB_API_BASE` so tests can point at a wiremock instance.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Sentinel placeholder string the user must replace before the
/// config is usable. Documented in the PR body and in the error message
/// the verb prints when a field still matches.
pub const PLACEHOLDER: &str = "REPLACE_WITH_";

/// User-Agent on every GitHub API call. GitHub rejects calls without
/// one; a stable string makes our traffic identifiable in audit logs.
const USER_AGENT: &str = "boss-shake";

/// JWT lifetime. GitHub allows up to 10 minutes; 9 gives clock-skew
/// slack on both ends.
const JWT_TTL_SECS: u64 = 9 * 60;

/// Where to find App credentials.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub app_id: String,
    pub installation_id: String,
    pub private_key_path: PathBuf,
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
/// The full response carries dozens of fields; we only need the URL
/// and the issue number for the success message.
#[derive(Debug, Deserialize)]
pub struct IssueResponse {
    pub html_url: String,
    pub number: u64,
}

/// Resolve the config file path, honoring the `BOSS_GITHUB_APP_CONFIG`
/// override.
pub fn config_path() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("BOSS_GITHUB_APP_CONFIG") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("$HOME is not set"))?;
    Ok(PathBuf::from(home).join(DEFAULT_CONFIG_REL))
}

/// Load and validate the App config. Fails loud if the file is missing
/// or any field still has a placeholder value — the user needs the
/// error message to point them at the setup instructions.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read GitHub App config at {}. See PR #748 for setup instructions.",
            path.display()
        )
    })?;
    let cfg: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("parse {} as TOML", path.display()))?;

    if cfg.app_id.is_empty() || cfg.app_id.contains(PLACEHOLDER) {
        bail!(
            "{}: app_id is unset or still a placeholder. See PR #748 for setup instructions.",
            path.display()
        );
    }
    if cfg.installation_id.is_empty() || cfg.installation_id.contains(PLACEHOLDER) {
        bail!(
            "{}: installation_id is unset or still a placeholder. See PR #748 for setup instructions.",
            path.display()
        );
    }
    let key_path = cfg.private_key_path.to_string_lossy();
    if key_path.is_empty() || key_path.contains(PLACEHOLDER) {
        bail!(
            "{}: private_key_path is unset or still a placeholder. See PR #748 for setup instructions.",
            path.display()
        );
    }

    Ok(cfg)
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
    let payload = CreateIssueBody {
        title,
        body,
        labels,
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

/// Top-level entry point used by the CLI: read the App config, sign a
/// JWT, swap it for an installation token, then file the issue. The
/// `api_base` is parametrized so tests can point at a wiremock; the CLI
/// caller passes [`DEFAULT_API_BASE`] (or whatever
/// `BOSS_GITHUB_API_BASE` resolves to).
pub async fn file_issue(
    config: &AppConfig,
    api_base: &str,
    repo: &str,
    title: &str,
    body: &str,
    labels: &[String],
) -> Result<IssueResponse> {
    let pem = std::fs::read(&config.private_key_path).with_context(|| {
        format!(
            "read GitHub App private key at {}",
            config.private_key_path.display()
        )
    })?;
    let jwt = build_jwt(&config.app_id, &pem)?;
    let client = http_client();
    let token = fetch_installation_token(api_base, &config.installation_id, &jwt, client).await?;
    create_issue(api_base, repo, title, body, labels, &token, client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation, decode};
    use std::io::Write;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A throwaway RSA-2048 keypair generated offline (`openssl genrsa
    /// 2048` + `openssl rsa -pubout`). Loaded via `include_str!` from
    /// `tests/fixtures/` so the actual armoring matches what GitHub's
    /// App download UI emits.
    const TEST_RSA_PEM: &str = include_str!("../tests/fixtures/github-app-private-key.pem");
    const TEST_RSA_PUBLIC_PEM: &str = include_str!("../tests/fixtures/github-app-public-key.pem");

    fn write_config(dir: &Path, key_path: &Path) -> PathBuf {
        let cfg_path = dir.join("github-app.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(
            f,
            "app_id = \"12345\"\ninstallation_id = \"67890\"\nprivate_key_path = \"{}\"",
            key_path.display()
        )
        .unwrap();
        cfg_path
    }

    fn write_private_key(dir: &Path) -> PathBuf {
        let key_path = dir.join("private-key.pem");
        std::fs::write(&key_path, TEST_RSA_PEM).unwrap();
        key_path
    }

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
    fn load_config_reads_valid_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let key = write_private_key(tmp.path());
        let cfg_path = write_config(tmp.path(), &key);

        let cfg = load_config(&cfg_path).unwrap();
        assert_eq!(cfg.app_id, "12345");
        assert_eq!(cfg.installation_id, "67890");
        assert_eq!(cfg.private_key_path, key);
    }

    #[test]
    fn load_config_rejects_missing_file() {
        let err = load_config(Path::new("/definitely/not/here/github-app.toml")).unwrap_err();
        assert!(
            err.to_string().contains("PR #748"),
            "error should point at setup instructions: {err}"
        );
    }

    #[test]
    fn load_config_rejects_placeholder_app_id() {
        let tmp = tempfile::tempdir().unwrap();
        let key = write_private_key(tmp.path());
        let cfg_path = tmp.path().join("github-app.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "app_id = \"REPLACE_WITH_APP_ID\"\ninstallation_id = \"67890\"\nprivate_key_path = \"{}\"",
                key.display()
            ),
        )
        .unwrap();

        let err = load_config(&cfg_path).unwrap_err();
        assert!(
            err.to_string().contains("app_id") && err.to_string().contains("placeholder"),
            "error should call out the placeholder: {err}"
        );
    }

    #[test]
    fn load_config_rejects_placeholder_installation_id() {
        let tmp = tempfile::tempdir().unwrap();
        let key = write_private_key(tmp.path());
        let cfg_path = tmp.path().join("github-app.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "app_id = \"12345\"\ninstallation_id = \"REPLACE_WITH_INSTALLATION_ID\"\nprivate_key_path = \"{}\"",
                key.display()
            ),
        )
        .unwrap();

        let err = load_config(&cfg_path).unwrap_err();
        assert!(
            err.to_string().contains("installation_id"),
            "error should call out installation_id: {err}"
        );
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
        // installation token. Returns a canned issue URL.
        Mock::given(method("POST"))
            .and(path("/repos/spinyfin/mono/issues"))
            .and(header("authorization", "Bearer v1.installation-token"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "html_url": "https://github.com/spinyfin/mono/issues/9999",
                "number": 9999,
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let key_path = write_private_key(tmp.path());
        let cfg = AppConfig {
            app_id: "42".into(),
            installation_id: "67890".into(),
            private_key_path: key_path,
        };

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

        let tmp = tempfile::tempdir().unwrap();
        let key_path = write_private_key(tmp.path());
        let cfg = AppConfig {
            app_id: "42".into(),
            installation_id: "67890".into(),
            private_key_path: key_path,
        };

        let err = file_issue(&cfg, &server.uri(), "spinyfin/mono", "t", "b", &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") && msg.contains("Bad credentials"),
            "error should surface github's 401 body: {msg}"
        );
    }
}
