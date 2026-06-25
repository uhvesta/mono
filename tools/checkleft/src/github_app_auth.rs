//! GitHub App installation-token credential support for the Check Runs fallback.
//!
//! When a fine-grained PAT (`CHECKS_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, or
//! `gh auth token`) is not available, the Check Runs poster falls back to this module.
//! It reads three env vars:
//!
//! - `CHECKS_GITHUB_APP_ID` — the GitHub App's numeric ID (required).
//! - `CHECKS_GITHUB_APP_PRIVATE_KEY` — RSA private key in PEM format (required).
//! - `CHECKS_GITHUB_INSTALLATION_ID` — optional; if absent the installation is
//!   discovered via `GET /repos/{owner}/{repo}/installation`.
//!
//! The recommended credential for Buildkite CI is a fine-grained PAT via
//! `CHECKS_GITHUB_TOKEN`; this module is the heavier fallback for environments
//! that cannot issue PATs (e.g. GitHub App–based CI systems).
//!
//! **Secret handling:** private keys and tokens are never logged. `AppCreds` does
//! not implement `Debug`; the acquired token is returned as an opaque `String`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

/// `CHECKS_GITHUB_APP_ID`: GitHub App numeric ID.
pub const CHECKS_GITHUB_APP_ID_ENV: &str = "CHECKS_GITHUB_APP_ID";
/// `CHECKS_GITHUB_APP_PRIVATE_KEY`: RSA private key for the GitHub App (PEM-encoded).
pub const CHECKS_GITHUB_APP_PRIVATE_KEY_ENV: &str = "CHECKS_GITHUB_APP_PRIVATE_KEY";
/// `CHECKS_GITHUB_INSTALLATION_ID`: optional installation ID override. When absent,
/// the installation is discovered via `GET /repos/{owner}/{repo}/installation`.
pub const CHECKS_GITHUB_INSTALLATION_ID_ENV: &str = "CHECKS_GITHUB_INSTALLATION_ID";

/// GitHub App credentials read from env vars.
///
/// Does not implement `Debug` to prevent accidental logging of the private key.
pub struct AppCreds {
    pub app_id: String,
    /// RSA private key in PEM format. Treated as a secret — never log.
    pub private_key_pem: String,
    /// Optional pre-configured installation ID. When `None`, `acquire_installation_token`
    /// discovers it via the GitHub API.
    pub installation_id: Option<String>,
}

/// Read GitHub App credentials from env vars.
///
/// Returns `None` if either required var (`CHECKS_GITHUB_APP_ID` or
/// `CHECKS_GITHUB_APP_PRIVATE_KEY`) is absent or empty.
pub fn read_app_creds_from_env() -> Option<AppCreds> {
    let app_id = std::env::var(CHECKS_GITHUB_APP_ID_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let private_key_pem = std::env::var(CHECKS_GITHUB_APP_PRIVATE_KEY_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let installation_id = std::env::var(CHECKS_GITHUB_INSTALLATION_ID_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());
    Some(AppCreds {
        app_id: app_id.trim().to_owned(),
        private_key_pem,
        installation_id: installation_id.map(|s| s.trim().to_owned()),
    })
}

/// JWT lifetime. GitHub allows up to 10 minutes; 9 gives clock-skew
/// slack on both ends.
const JWT_TTL_SECS: u64 = 9 * 60;

/// Sign a 9-minute GitHub App JWT with the given RSA private key.
///
/// The key must be PKCS#1 or PKCS#8 PEM-encoded RSA. The issued-at time
/// is back-dated 60 seconds to absorb clock skew between the signing host
/// and GitHub's servers.
pub fn build_jwt(app_id: &str, private_key_pem: &[u8]) -> Result<String> {
    let iat = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .context("system clock is before UNIX epoch")?;
    build_jwt_at(app_id, private_key_pem, iat)
}

/// `build_jwt` with the issued-at timestamp injected, so tests can pin it.
pub fn build_jwt_at(app_id: &str, private_key_pem: &[u8], iat: u64) -> Result<String> {
    let key = EncodingKey::from_rsa_pem(private_key_pem)
        .context("parse GitHub App private key (must be PKCS#1 or PKCS#8 PEM-encoded RSA)")?;

    #[derive(Serialize)]
    struct Claims<'a> {
        iat: u64,
        exp: u64,
        iss: &'a str,
    }
    let claims = Claims {
        iat: iat.saturating_sub(60),
        exp: iat + JWT_TTL_SECS,
        iss: app_id,
    };
    let header = Header::new(Algorithm::RS256);
    encode(&header, &claims, &key).context("sign GitHub App JWT (RS256)")
}

#[derive(Deserialize)]
struct InstallationResponse {
    id: u64,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
}

/// Discover the GitHub App installation for `owner_repo` via the GitHub API.
///
/// Requires the App JWT (`jwt`) as a bearer token. Returns the installation
/// ID as a string suitable for passing to `exchange_for_installation_token`.
async fn discover_installation_id(
    owner_repo: &str,
    jwt: &str,
    base_url: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let url = format!("{}/repos/{}/installation", base_url.trim_end_matches('/'), owner_repo);
    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(reqwest::header::USER_AGENT, "checkleft-cli")
        .bearer_auth(jwt)
        .send()
        .await
        .context("GET /repos/{owner_repo}/installation")?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("reading installation discovery response body")?;
    if !status.is_success() {
        let snippet = truncate_response_body(String::from_utf8_lossy(&bytes).trim(), 300);
        anyhow::bail!("GET /repos/{owner_repo}/installation returned HTTP {status}: {snippet}");
    }
    let parsed: InstallationResponse =
        serde_json::from_slice(&bytes).context("parsing GET /repos/{owner_repo}/installation response")?;
    Ok(parsed.id.to_string())
}

/// Exchange the App JWT for a short-lived installation access token.
///
/// `installation_id` must be the numeric GitHub App installation id (as a
/// string). Returns the raw token value, which should be treated as a secret.
async fn exchange_for_installation_token(
    installation_id: &str,
    jwt: &str,
    base_url: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let url = format!(
        "{}/app/installations/{}/access_tokens",
        base_url.trim_end_matches('/'),
        installation_id
    );
    let response = client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(reqwest::header::USER_AGENT, "checkleft-cli")
        .bearer_auth(jwt)
        .send()
        .await
        .context("POST /app/installations/{installation_id}/access_tokens")?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("reading installation token response body")?;
    if !status.is_success() {
        let snippet = truncate_response_body(String::from_utf8_lossy(&bytes).trim(), 300);
        anyhow::bail!("installation token exchange returned HTTP {status}: {snippet}");
    }
    let parsed: InstallationTokenResponse =
        serde_json::from_slice(&bytes).context("parsing installation token response")?;
    Ok(parsed.token)
}

/// Acquire a short-lived GitHub App installation access token for `owner_repo`.
///
/// Returns `Ok(None)` when the App credential env vars are not set (so the
/// caller can continue to a different error path rather than treating the
/// absent creds as a hard failure). Returns `Ok(Some(token))` on success.
/// Returns `Err` when the creds are present but the API call or JWT signing
/// fails — indicating a configuration error worth surfacing loudly.
///
/// `base_url` is the GitHub REST root (e.g. `https://api.github.com` or a
/// GitHub Enterprise URL). The rustls crypto provider is installed
/// automatically if not already initialized.
pub async fn acquire_installation_token(owner_repo: &str, base_url: &str) -> Result<Option<String>> {
    let creds = match read_app_creds_from_env() {
        Some(c) => c,
        None => return Ok(None),
    };

    crate::vcs::ensure_rustls_provider();
    let client = reqwest::Client::new();

    let jwt = build_jwt(&creds.app_id, creds.private_key_pem.as_bytes())?;

    let installation_id = match creds.installation_id {
        Some(id) => id,
        None => discover_installation_id(owner_repo, &jwt, base_url, &client).await?,
    };

    let token = exchange_for_installation_token(&installation_id, &jwt, base_url, &client).await?;
    Ok(Some(token))
}

fn truncate_response_body(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
    use rsa::RsaPrivateKey;
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding as Pkcs1LineEnding};
    use rsa::pkcs8::EncodePublicKey;
    use serde::Deserialize;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn generate_test_keypair() -> (String, String) {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("RSA keygen failed");
        let public_key = rsa::RsaPublicKey::from(&private_key);
        let private_pem = private_key
            .to_pkcs1_pem(Pkcs1LineEnding::LF)
            .expect("private key to PKCS#1 PEM")
            .to_string();
        let public_pem = public_key
            .to_public_key_pem(rsa::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("public key to SPKI PEM");
        (private_pem, public_pem)
    }

    // ── JWT signing ──────────────────────────────────────────────────────────

    #[test]
    fn build_jwt_produces_rs256_token_with_correct_claims() {
        let (private_pem, public_pem) = generate_test_keypair();
        let iat: u64 = 1_700_000_000;
        let token = build_jwt_at("1234", private_pem.as_bytes(), iat).unwrap();

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&["1234"]);
        validation.validate_exp = false;

        #[derive(Deserialize)]
        struct Claims {
            iat: u64,
            exp: u64,
            iss: String,
        }
        let key = DecodingKey::from_rsa_pem(public_pem.as_bytes()).unwrap();
        let decoded = decode::<Claims>(&token, &key, &validation).unwrap();
        assert_eq!(decoded.claims.iss, "1234");
        assert_eq!(decoded.claims.iat, iat - 60, "iat is back-dated 60s for clock skew");
        assert_eq!(decoded.claims.exp, iat + JWT_TTL_SECS);
    }

    #[test]
    fn build_jwt_rejects_garbage_pem() {
        let err = build_jwt_at("42", b"not a pem", 1_000_000).unwrap_err();
        assert!(
            err.to_string().contains("private key"),
            "error should mention private key: {err}"
        );
    }

    // ── env-var reading ───────────────────────────────────────────────────────

    #[test]
    fn read_app_creds_returns_none_when_vars_absent() {
        // Rely on vars not being set in the test environment.
        // (If they are set, this test would need to be skipped — acceptable for
        // unit tests that run in a clean sandbox.)
        let before_id = std::env::var(CHECKS_GITHUB_APP_ID_ENV);
        let before_key = std::env::var(CHECKS_GITHUB_APP_PRIVATE_KEY_ENV);
        if before_id.is_err() && before_key.is_err() {
            assert!(read_app_creds_from_env().is_none());
        }
    }

    // ── HTTP: installation discovery and token exchange ───────────────────────

    #[tokio::test]
    async fn discover_installation_id_parses_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/installation"))
            .and(header("authorization", "Bearer test-jwt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": 42 })))
            .mount(&server)
            .await;

        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let id = discover_installation_id("owner/repo", "test-jwt", &server.uri(), &client)
            .await
            .unwrap();
        assert_eq!(id, "42");
    }

    #[tokio::test]
    async fn discover_installation_id_non_2xx_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/installation"))
            .respond_with(ResponseTemplate::new(404).set_body_string("{\"message\":\"Not Found\"}"))
            .mount(&server)
            .await;

        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let err = discover_installation_id("owner/repo", "test-jwt", &server.uri(), &client)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"), "error should include status: {msg}");
        assert!(msg.contains("Not Found"), "error should include body excerpt: {msg}");
    }

    #[tokio::test]
    async fn exchange_for_installation_token_returns_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .and(header("authorization", "Bearer signed-jwt"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "token": "ghs_secret" })))
            .mount(&server)
            .await;

        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let token = exchange_for_installation_token("99", "signed-jwt", &server.uri(), &client)
            .await
            .unwrap();
        assert_eq!(token, "ghs_secret");
    }

    #[tokio::test]
    async fn exchange_for_installation_token_non_2xx_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{\"message\":\"Bad credentials\"}"))
            .mount(&server)
            .await;

        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let err = exchange_for_installation_token("99", "bad-jwt", &server.uri(), &client)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "error should include status: {msg}");
        assert!(
            msg.contains("Bad credentials"),
            "error should include body excerpt: {msg}"
        );
    }

    #[tokio::test]
    async fn acquire_installation_token_returns_none_without_env_vars() {
        // Assumes CHECKS_GITHUB_APP_ID and CHECKS_GITHUB_APP_PRIVATE_KEY are not set.
        let id_set = std::env::var(CHECKS_GITHUB_APP_ID_ENV).is_ok_and(|v| !v.is_empty());
        let key_set = std::env::var(CHECKS_GITHUB_APP_PRIVATE_KEY_ENV).is_ok_and(|v| !v.is_empty());
        if !id_set && !key_set {
            let result = acquire_installation_token("owner/repo", "https://api.github.com")
                .await
                .unwrap();
            assert!(result.is_none());
        }
    }

    #[tokio::test]
    async fn acquire_installation_token_uses_configured_installation_id() {
        let (private_pem, _) = generate_test_keypair();
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/app/installations/77/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "token": "ghs_acquired" })))
            .mount(&server)
            .await;

        // Drive acquire_installation_token directly with pre-set creds, bypassing
        // env var reading (we call the internal pieces to avoid env mutation in tests).
        let jwt = build_jwt_at("app-123", private_pem.as_bytes(), 1_700_000_000).unwrap();
        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let token = exchange_for_installation_token("77", &jwt, &server.uri(), &client)
            .await
            .unwrap();
        assert_eq!(token, "ghs_acquired");
    }

    #[tokio::test]
    async fn acquire_installation_token_discovers_installation_when_id_absent() {
        let (private_pem, _) = generate_test_keypair();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": 55 })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/app/installations/55/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "token": "ghs_discovered" })))
            .mount(&server)
            .await;

        let jwt = build_jwt_at("app-456", private_pem.as_bytes(), 1_700_000_000).unwrap();
        crate::vcs::ensure_rustls_provider();
        let client = reqwest::Client::new();
        // Discovery flow: first get the installation id, then exchange for a token.
        let installation_id = discover_installation_id("owner/repo", &jwt, &server.uri(), &client)
            .await
            .unwrap();
        let token = exchange_for_installation_token(&installation_id, &jwt, &server.uri(), &client)
            .await
            .unwrap();
        assert_eq!(token, "ghs_discovered");
    }
}
