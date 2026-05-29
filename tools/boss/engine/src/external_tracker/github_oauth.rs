//! GitHub OAuth device-flow client and auth state machine.
//!
//! This module implements the engine side of the OAuth device authorization
//! flow described in `tools/boss/docs/designs/oauth-device-flow-auth-for-issue-sync.md`
//! §2 (device-flow client) and §3 (auth state machine).
//!
//! The public surface:
//! - [`DeviceFlow`] — pure HTTP layer: request device code, poll for token,
//!   validate token, probe org/SSO state.
//! - [`GitHubAuthState`] — engine-internal state enum (carries the private
//!   `device_code`; converts to [`GitHubAuthStateDto`] for the wire).
//! - [`GitHubAuthController`] — state machine driver that T-4 uses from
//!   `app.rs` to handle `GitHubAuthStart/Cancel/Disconnect/Status` RPCs.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::time::sleep;

use boss_protocol::{GitHubAuthStateDto, OrgAuthState};

// ── Client-id + GitHub endpoint constants ────────────────────────────────────

/// Production spinyfin OAuth App client_id (non-secret; device flow does not
/// require a client secret).  Recorded canonically on T-0/T767.
pub const CLIENT_ID: &str = "Ov23li9VOztDIjoOA7eW";

// Pre-encoded form values (no external urlencode dep needed).
// Scope "repo project" → space becomes "+" in application/x-www-form-urlencoded.
const SCOPE_ENCODED: &str = "repo+project";
// grant_type URN with colons percent-encoded.
const GRANT_TYPE_ENCODED: &str =
    "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code";

const DEFAULT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const DEFAULT_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEFAULT_USER_URL: &str = "https://api.github.com/user";
const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Per GitHub spec: each `slow_down` response increases the minimum poll
/// interval by this many seconds.
const SLOW_DOWN_BACKOFF_SECS: u64 = 5;

/// Grace period beyond `expires_in` before the poll loop hard-stops.
const EXPIRY_GRACE_SECS: u64 = 5;

// ── Configurable URLs (injectable for testing) ────────────────────────────────

/// Endpoint URLs used by [`DeviceFlow`].  Default points at github.com;
/// tests override these to aim at a local [`wiremock::MockServer`].
#[derive(Debug, Clone)]
pub struct DeviceFlowConfig {
    pub client_id: String,
    pub device_code_url: String,
    pub token_url: String,
    pub user_url: String,
    pub api_base_url: String,
}

impl Default for DeviceFlowConfig {
    fn default() -> Self {
        Self {
            client_id: CLIENT_ID.to_owned(),
            device_code_url: DEFAULT_DEVICE_CODE_URL.to_owned(),
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            user_url: DEFAULT_USER_URL.to_owned(),
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
        }
    }
}

// ── GitHub API response shapes ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

/// Untagged so serde picks the right variant by field presence.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPollResponse {
    Success(RawPollSuccess),
    Failure(RawPollFailure),
}

#[derive(Debug, Deserialize)]
struct RawPollSuccess {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct RawPollFailure {
    error: String,
}

#[derive(Debug, Deserialize)]
struct RawUserResponse {
    login: String,
}

// ── Public data types ─────────────────────────────────────────────────────────

/// Information returned from the device-code request step.
/// The `device_code` is a bearer-equivalent secret kept internal to the engine;
/// the UI only ever sees `user_code` and `verification_uri` via the DTO.
#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    /// Bearer-equivalent secret — never send to the UI.
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    /// Unix epoch seconds when the device code expires.
    pub expires_at: i64,
    pub interval_secs: u64,
}

/// Captured token with identity metadata.  Persisted in the macOS keychain
/// by [`KeychainTokenStore`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub token: String,
    pub login: String,
    pub granted_scopes: Vec<String>,
    pub obtained_at: i64,
}

/// Outcome of [`DeviceFlow::poll_for_token`].
pub enum PollOutcome {
    /// Token acquired and validated.
    Authorized(TokenRecord),
    /// Device code expired before the user completed authorization.
    Expired,
    /// User denied the authorization request.
    Denied,
    /// Caller cancelled via the cancel channel.
    Cancelled,
    /// Non-recoverable error (programming/config error or unexpected response).
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GitHub API error: {code}")]
    GitHubError { code: String },
}

// ── DeviceFlow ────────────────────────────────────────────────────────────────

/// Pure HTTP layer for the OAuth device authorization flow.
///
/// Stateless: it drives HTTP calls and returns results.  The [`GitHubAuthController`]
/// drives this and manages the state machine.
pub struct DeviceFlow {
    config: DeviceFlowConfig,
    client: reqwest::Client,
}

impl DeviceFlow {
    pub fn new(config: DeviceFlowConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }

    /// Convenience constructor for production use (points at github.com,
    /// uses the canonical `CLIENT_ID`).
    pub fn production(client: reqwest::Client) -> Self {
        Self::new(DeviceFlowConfig::default(), client)
    }

    /// **Step 1** — POST to `/login/device/code` and return the device-code info.
    pub async fn request_device_code(&self) -> Result<DeviceCodeInfo, FlowError> {
        let form_body = format!(
            "client_id={}&scope={}",
            self.config.client_id, SCOPE_ENCODED
        );
        let raw: RawDeviceCodeResponse = self
            .client
            .post(&self.config.device_code_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let expires_at = unix_now() + raw.expires_in as i64;

        Ok(DeviceCodeInfo {
            device_code: raw.device_code,
            user_code: raw.user_code,
            verification_uri: raw.verification_uri,
            verification_uri_complete: raw.verification_uri_complete,
            expires_at,
            interval_secs: raw.interval,
        })
    }

    /// **Step 3** — Poll `/login/oauth/access_token` until the user authorizes
    /// (or the code expires / user denies / caller cancels).
    ///
    /// Honors the `interval` / `slow_down` / `authorization_pending` /
    /// `expired_token` / `access_denied` error codes from GitHub's spec.
    /// Network errors and HTTP 5xx are treated as transient and do not abort
    /// the loop.  A hard wall-clock cap at `expires_at + EXPIRY_GRACE_SECS`
    /// prevents spinning forever.
    ///
    /// The loop returns `PollOutcome::Cancelled` as soon as `cancel_rx`
    /// carries `true` (checked before every sleep and every poll request).
    pub async fn poll_for_token(
        &self,
        device_code: &str,
        initial_interval_secs: u64,
        expires_at: i64,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> PollOutcome {
        let mut interval_secs = initial_interval_secs;
        let hard_deadline = expires_at + EXPIRY_GRACE_SECS as i64;

        loop {
            // --- Cancellation / expiry check before sleeping ----------------
            if *cancel_rx.borrow() {
                return PollOutcome::Cancelled;
            }
            if unix_now() >= hard_deadline {
                return PollOutcome::Expired;
            }

            // --- Sleep for the current interval, interruptible by cancel ---
            tokio::select! {
                _ = sleep(Duration::from_secs(interval_secs)) => {}
                result = cancel_rx.changed() => {
                    if result.is_ok() && *cancel_rx.borrow() {
                        return PollOutcome::Cancelled;
                    }
                }
            }

            // --- Post-sleep checks -----------------------------------------
            if *cancel_rx.borrow() {
                return PollOutcome::Cancelled;
            }
            if unix_now() >= hard_deadline {
                return PollOutcome::Expired;
            }

            // --- Poll GitHub for the token ---------------------------------
            let poll_body = format!(
                "client_id={}&device_code={}&grant_type={}",
                self.config.client_id, device_code, GRANT_TYPE_ENCODED
            );
            let http_result = self
                .client
                .post(&self.config.token_url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(poll_body)
                .send()
                .await;

            let response = match http_result {
                Err(e) => {
                    // Network error: transient; keep polling.
                    tracing::warn!(
                        target: "boss_engine::external_tracker::github_oauth",
                        error = %e,
                        "device flow poll: network error; retrying on next interval"
                    );
                    continue;
                }
                Ok(r) => r,
            };

            if response.status().is_server_error() {
                tracing::warn!(
                    target: "boss_engine::external_tracker::github_oauth",
                    status = %response.status(),
                    "device flow poll: server error; retrying on next interval"
                );
                continue;
            }

            let body: RawPollResponse = match response.json().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        target: "boss_engine::external_tracker::github_oauth",
                        error = %e,
                        "device flow poll: unexpected response body; retrying"
                    );
                    continue;
                }
            };

            match body {
                RawPollResponse::Success(s) => {
                    return match self.validate_token(&s.access_token).await {
                        Ok(record) => PollOutcome::Authorized(record),
                        Err(e) => PollOutcome::Error(e.to_string()),
                    };
                }
                RawPollResponse::Failure(f) => match f.error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        interval_secs += SLOW_DOWN_BACKOFF_SECS;
                        continue;
                    }
                    "expired_token" => return PollOutcome::Expired,
                    "access_denied" => return PollOutcome::Denied,
                    other => {
                        tracing::error!(
                            target: "boss_engine::external_tracker::github_oauth",
                            error_code = other,
                            "device flow poll: non-recoverable GitHub error"
                        );
                        return PollOutcome::Error(format!("GitHub error: {other}"));
                    }
                },
            }
        }
    }

    /// **Step 4** — Validate a captured token by calling `GET /user`.
    ///
    /// Reads the `X-OAuth-Scopes` response header to record what scopes GitHub
    /// actually granted (may be a subset of what was requested).
    pub async fn validate_token(&self, token: &str) -> Result<TokenRecord, FlowError> {
        let response = self
            .client
            .get(&self.config.user_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", "boss-engine")
            .send()
            .await?
            .error_for_status()?;

        let granted_scopes: Vec<String> = response
            .headers()
            .get("X-OAuth-Scopes")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();

        let user: RawUserResponse = response.json().await?;

        Ok(TokenRecord {
            token: token.to_owned(),
            login: user.login,
            granted_scopes,
            obtained_at: unix_now(),
        })
    }

    /// **Org/SSO probe** — issue a lightweight API call against an org resource
    /// to determine whether the token can reach private org resources.
    ///
    /// Classifies the result into [`OrgAuthState`]:
    /// - 200 → `Ok`
    /// - 403 with `X-GitHub-SSO: required; url=<...>` → `NeedsSso`
    /// - 403 without SSO header → `NeedsOrgApproval`
    /// - network/parse error → `Unknown`
    ///
    /// If `org` is `None`, returns `Unknown` (caller must supply the org from
    /// the bound product's tracker config; T-4 is responsible for this).
    pub async fn probe_org_state(&self, token: &str, org: Option<&str>) -> OrgAuthState {
        let Some(org) = org else {
            return OrgAuthState::Unknown;
        };

        let url = format!("{}/orgs/{org}", self.config.api_base_url);
        let result = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", "boss-engine")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await;

        let response = match result {
            Err(e) => {
                tracing::warn!(
                    target: "boss_engine::external_tracker::github_oauth",
                    %org, error = %e,
                    "org/SSO probe: network error"
                );
                return OrgAuthState::Unknown;
            }
            Ok(r) => r,
        };

        if response.status().is_success() {
            return OrgAuthState::Ok;
        }

        if response.status() == 403 {
            // Check for SAML SSO requirement header.
            if let Some(sso_header) = response.headers().get("X-GitHub-SSO") {
                if let Ok(s) = sso_header.to_str() {
                    if s.contains("required") {
                        // Header format: "required; url=https://github.com/orgs/foo/sso?..."
                        if let Some(url_part) = s.split("url=").nth(1) {
                            return OrgAuthState::NeedsSso {
                                sso_url: url_part.trim().to_owned(),
                            };
                        }
                    }
                }
            }
            // 403 without SSO header: OAuth App not yet approved for this org.
            return OrgAuthState::NeedsOrgApproval {
                request_url: format!(
                    "https://github.com/orgs/{org}/policies/applications"
                ),
            };
        }

        // Any other status (401, 404, etc.) — inconclusive.
        tracing::warn!(
            target: "boss_engine::external_tracker::github_oauth",
            %org, status = %response.status(),
            "org/SSO probe: unexpected status"
        );
        OrgAuthState::Unknown
    }
}

// ── GitHubAuthState (engine-internal) ────────────────────────────────────────

/// Engine-internal representation of GitHub OAuth auth state.
///
/// Richer than [`GitHubAuthStateDto`]: carries the private `device_code` (a
/// bearer-equivalent secret never included in the wire DTO) and the full
/// [`TokenRecord`].  Convert to the wire type with [`GitHubAuthState::to_dto`].
#[derive(Debug, Clone)]
pub enum GitHubAuthState {
    /// No stored token; no flow in progress.
    Disconnected,
    /// Requesting the device + user code from GitHub.
    RequestingCode,
    /// Device code obtained; waiting for the user to authorize in their browser.
    PendingUserAuth {
        /// Private; never sent to UI.
        device_code: String,
        user_code: String,
        verification_uri: String,
        verification_uri_complete: Option<String>,
        expires_at: i64,
        interval_secs: u64,
    },
    /// Token obtained, validated, and (after T-3) stored in keychain.
    Authorized {
        record: TokenRecord,
        org_state: OrgAuthState,
    },
    /// Device code expired before the user completed authorization.
    Expired,
    /// User denied the authorization request.
    Denied,
    /// Non-recoverable error during the flow.
    Error { message: String },
}

impl GitHubAuthState {
    /// Convert to the wire DTO.  The `device_code` and raw token are excluded.
    pub fn to_dto(&self) -> GitHubAuthStateDto {
        match self {
            Self::Disconnected => GitHubAuthStateDto::Disconnected,
            Self::RequestingCode => GitHubAuthStateDto::RequestingCode,
            Self::PendingUserAuth {
                user_code,
                verification_uri,
                verification_uri_complete,
                expires_at,
                interval_secs,
                ..
            } => GitHubAuthStateDto::PendingUserAuth {
                user_code: user_code.clone(),
                verification_uri: verification_uri.clone(),
                verification_uri_complete: verification_uri_complete.clone(),
                expires_at: *expires_at,
                interval_seconds: *interval_secs as u32,
            },
            Self::Authorized { record, org_state } => GitHubAuthStateDto::Authorized {
                login: record.login.clone(),
                granted_scopes: record.granted_scopes.clone(),
                org_state: org_state.clone(),
            },
            Self::Expired => GitHubAuthStateDto::Expired,
            Self::Denied => GitHubAuthStateDto::Denied,
            Self::Error { message } => GitHubAuthStateDto::Error { message: message.clone() },
        }
    }

    /// Returns the stored [`TokenRecord`] if in `Authorized` state.
    pub fn token_record(&self) -> Option<&TokenRecord> {
        if let Self::Authorized { record, .. } = self { Some(record) } else { None }
    }
}

// ── GitHubAuthController ──────────────────────────────────────────────────────

/// State machine controller that T-4 (`app.rs`) uses to handle
/// `GitHubAuthStart / Cancel / Disconnect / Status` RPCs.
///
/// # Concurrency model
/// - `start_flow()` spawns a Tokio task that drives [`DeviceFlow`] and sends
///   state updates through an internal `watch` channel.
/// - Callers obtain a `watch::Receiver<GitHubAuthState>` at construction and
///   can `subscribe()` to new receivers; they react to state changes by pushing
///   [`GitHubAuthStateDto`] events to the app frontend.
/// - `cancel()` and `disconnect()` are synchronous (no await) from the caller's
///   perspective — they signal the background task and update state immediately.
pub struct GitHubAuthController {
    state_tx: watch::Sender<GitHubAuthState>,
    cancel_slot: Arc<Mutex<Option<watch::Sender<bool>>>>,
    flow: Arc<DeviceFlow>,
}

impl GitHubAuthController {
    /// Create a new controller.  Returns the controller and an initial state
    /// receiver that the caller can watch for state-change notifications.
    pub fn new(flow: DeviceFlow) -> (Self, watch::Receiver<GitHubAuthState>) {
        let (tx, rx) = watch::channel(GitHubAuthState::Disconnected);
        let ctrl = Self {
            state_tx: tx,
            cancel_slot: Arc::new(Mutex::new(None)),
            flow: Arc::new(flow),
        };
        (ctrl, rx)
    }

    /// Subscribe to state changes.  Each call returns a new receiver starting
    /// at the current state.
    pub fn subscribe(&self) -> watch::Receiver<GitHubAuthState> {
        self.state_tx.subscribe()
    }

    /// Current state snapshot.
    pub fn current_state(&self) -> GitHubAuthState {
        self.state_tx.borrow().clone()
    }

    /// Begin the device-flow.  Cancels any in-progress flow, transitions to
    /// `RequestingCode`, then spawns a task that drives the full flow.
    ///
    /// State updates are broadcast on the watch channel; T-4 listens and
    /// forwards them as `FrontendEvent::GitHubAuthState` events.
    pub async fn start_flow(&self) {
        // Cancel any running flow.
        self.signal_cancel().await;

        // Immediately transition to RequestingCode.
        self.state_tx.send_replace(GitHubAuthState::RequestingCode);

        // Create a fresh cancel channel for this flow.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        *self.cancel_slot.lock().await = Some(cancel_tx);

        let flow = Arc::clone(&self.flow);
        let state_tx = self.state_tx.clone();

        tokio::spawn(async move {
            // Step 1: request device + user code.
            let device_info = match flow.request_device_code().await {
                Ok(info) => info,
                Err(e) => {
                    tracing::error!(
                        target: "boss_engine::external_tracker::github_oauth",
                        error = %e,
                        "device flow: failed to request device code"
                    );
                    state_tx
                        .send_replace(GitHubAuthState::Error { message: e.to_string() });
                    return;
                }
            };

            // Transition to PendingUserAuth so the UI can show the user code.
            state_tx.send_replace(GitHubAuthState::PendingUserAuth {
                device_code: device_info.device_code.clone(),
                user_code: device_info.user_code.clone(),
                verification_uri: device_info.verification_uri.clone(),
                verification_uri_complete: device_info.verification_uri_complete.clone(),
                expires_at: device_info.expires_at,
                interval_secs: device_info.interval_secs,
            });

            // Step 3: poll for the token.
            let outcome = flow
                .poll_for_token(
                    &device_info.device_code,
                    device_info.interval_secs,
                    device_info.expires_at,
                    cancel_rx,
                )
                .await;

            // Transition based on poll outcome.
            let new_state = match outcome {
                PollOutcome::Authorized(record) => {
                    // T-3 will add keychain persistence here.
                    // Org state is Unknown until T-4 runs probe_org_state().
                    GitHubAuthState::Authorized {
                        record,
                        org_state: OrgAuthState::Unknown,
                    }
                }
                PollOutcome::Expired => GitHubAuthState::Expired,
                PollOutcome::Denied => GitHubAuthState::Denied,
                PollOutcome::Cancelled => GitHubAuthState::Disconnected,
                PollOutcome::Error(msg) => GitHubAuthState::Error { message: msg },
            };

            state_tx.send_replace(new_state);
        });
    }

    /// Abort an in-progress flow.  The background task transitions to
    /// `Disconnected` once it observes the cancel signal.
    pub async fn cancel(&self) {
        self.signal_cancel().await;
    }

    /// Disconnect: immediately transition to `Disconnected` and delete any
    /// stored token.  T-3 will add the keychain deletion here.
    pub async fn disconnect(&self) {
        self.signal_cancel().await;
        self.state_tx.send_replace(GitHubAuthState::Disconnected);
    }

    /// Update the `org_state` sub-field of an `Authorized` state.
    /// T-4 calls this after running [`DeviceFlow::probe_org_state`].
    pub fn update_org_state(&self, org_state: OrgAuthState) {
        self.state_tx.send_if_modified(|s| {
            if let GitHubAuthState::Authorized { org_state: os, .. } = s {
                *os = org_state;
                true
            } else {
                false
            }
        });
    }

    // Send `true` to the cancel channel of any running flow task.
    async fn signal_cancel(&self) {
        if let Some(tx) = self.cancel_slot.lock().await.take() {
            let _ = tx.send(true);
        }
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── KeychainTokenStore ────────────────────────────────────────────────────────

/// OS keychain coordinates for the stored OAuth token.
pub(crate) const KEYCHAIN_SERVICE: &str = "dev.spinyfin.boss.github";
pub(crate) const KEYCHAIN_ACCOUNT: &str = "oauth-user-token@github.com";

/// Error type for [`KeychainTokenStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    #[error("token record (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Low-level storage backend abstraction.  The production impl uses
/// [`keyring::Entry`]; tests inject [`FakeStore`] to avoid touching the
/// real keychain.
pub(crate) trait KeystoreBackend: Send + Sync {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError>;
    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError>;
    fn delete_raw(&self) -> Result<(), TokenStoreError>;
}

/// Production backend: delegates to the OS keychain via `keyring::Entry`.
struct KeyringBackend;

impl KeystoreBackend for KeyringBackend {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(TokenStoreError::Keychain(e)),
        }
    }

    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        entry.set_password(value).map_err(TokenStoreError::Keychain)
    }

    fn delete_raw(&self) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(TokenStoreError::Keychain(e)),
        }
    }
}

/// Stores and retrieves a [`TokenRecord`] in the OS keychain.
///
/// The value at rest is a JSON blob serialised from / into [`TokenRecord`].
/// Production code constructs this with [`KeychainTokenStore::new`]; tests
/// supply a [`FakeStore`] via [`KeychainTokenStore::with_backend`].
pub struct KeychainTokenStore {
    backend: Box<dyn KeystoreBackend>,
}

impl KeychainTokenStore {
    /// Creates a store backed by the real OS keychain.
    pub fn new() -> Self {
        Self { backend: Box::new(KeyringBackend) }
    }

    /// Creates a store backed by the given test fake.  Only available in
    /// `#[cfg(test)]` builds.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: impl KeystoreBackend + 'static) -> Self {
        Self { backend: Box::new(backend) }
    }

    /// Returns the stored [`TokenRecord`], or `None` if no token is present.
    pub fn get(&self) -> Result<Option<TokenRecord>, TokenStoreError> {
        match self.backend.get_raw()? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    /// Persists a [`TokenRecord`] in the keychain, overwriting any prior value.
    pub fn set(&self, record: &TokenRecord) -> Result<(), TokenStoreError> {
        let s = serde_json::to_string(record)?;
        self.backend.set_raw(&s)
    }

    /// Removes the stored token.  A no-op if none is present.
    pub fn delete(&self) -> Result<(), TokenStoreError> {
        self.backend.delete_raw()
    }
}

// ── FakeStore (test-only) ─────────────────────────────────────────────────────

/// In-memory [`KeystoreBackend`] for tests.  Never touches the real keychain.
#[cfg(test)]
pub(crate) struct FakeStore(std::sync::Mutex<Option<String>>);

#[cfg(test)]
impl FakeStore {
    pub(crate) fn empty() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    pub(crate) fn prefilled(record: &TokenRecord) -> Self {
        let s = serde_json::to_string(record).expect("TokenRecord should serialize");
        Self(std::sync::Mutex::new(Some(s)))
    }
}

#[cfg(test)]
impl KeystoreBackend for FakeStore {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = Some(value.to_owned());
        Ok(())
    }

    fn delete_raw(&self) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_record() -> TokenRecord {
        TokenRecord {
            token: "gho_sample".to_owned(),
            login: "octocat".to_owned(),
            granted_scopes: vec!["repo".to_owned(), "project".to_owned()],
            obtained_at: 1_700_000_000,
        }
    }

    #[test]
    fn keychain_store_round_trips_token_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        assert!(store.get().unwrap().is_none());

        let record = sample_record();
        store.set(&record).unwrap();

        let got = store.get().unwrap().expect("should have a record");
        assert_eq!(got.token, record.token);
        assert_eq!(got.login, record.login);
        assert_eq!(got.granted_scopes, record.granted_scopes);
        assert_eq!(got.obtained_at, record.obtained_at);
    }

    #[test]
    fn keychain_store_delete_removes_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record()));
        assert!(store.get().unwrap().is_some());

        store.delete().unwrap();
        assert!(store.get().unwrap().is_none());
    }

    #[test]
    fn keychain_store_delete_is_idempotent_when_empty() {
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        store.delete().unwrap(); // should not error
    }

    #[test]
    fn keychain_store_set_overwrites_existing_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record()));
        let new_record = TokenRecord {
            token: "gho_new_token".to_owned(),
            login: "newuser".to_owned(),
            granted_scopes: vec!["repo".to_owned()],
            obtained_at: 1_800_000_000,
        };
        store.set(&new_record).unwrap();

        let got = store.get().unwrap().expect("should have a record");
        assert_eq!(got.token, "gho_new_token");
        assert_eq!(got.login, "newuser");
    }

    // Install rustls crypto provider once per test process.
    fn test_client() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new()
    }

    fn config_for(server: &MockServer) -> DeviceFlowConfig {
        DeviceFlowConfig {
            client_id: "test-client-id".to_owned(),
            device_code_url: format!("{}/login/device/code", server.uri()),
            token_url: format!("{}/login/oauth/access_token", server.uri()),
            user_url: format!("{}/user", server.uri()),
            api_base_url: server.uri().to_owned(),
        }
    }

    fn no_cancel() -> watch::Receiver<bool> {
        let (_tx, rx) = watch::channel(false);
        rx
    }

    fn user_mock(login: &str, scopes: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "login": login, "id": 1 }))
            .append_header("X-OAuth-Scopes", scopes)
    }

    // ── poll_for_token tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn poll_returns_authorized_on_immediate_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "gho_test_token",
                    "token_type": "bearer"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("testuser", "repo, project"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        let PollOutcome::Authorized(rec) = outcome else {
            panic!("expected Authorized");
        };
        assert_eq!(rec.login, "testuser");
        assert_eq!(rec.granted_scopes, vec!["repo", "project"]);
        assert_eq!(rec.token, "gho_test_token");
    }

    #[tokio::test]
    async fn poll_retries_on_authorization_pending_then_succeeds() {
        let server = MockServer::start().await;

        // First two calls: authorization_pending.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "error": "authorization_pending"
                })),
            )
            .up_to_n_times(2)
            .mount(&server)
            .await;

        // Third call: success.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "gho_tok_after_pending",
                    "token_type": "bearer"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("alice", "repo, project"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(
            matches!(outcome, PollOutcome::Authorized(ref r) if r.login == "alice"),
            "expected Authorized(alice)"
        );
    }

    #[tokio::test]
    async fn poll_handles_slow_down_and_eventually_succeeds() {
        let server = MockServer::start().await;

        // First call: slow_down.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "slow_down" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call: success.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "gho_tok_after_slowdown",
                    "token_type": "bearer"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("bob", "repo"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        // After slow_down, interval grows by SLOW_DOWN_BACKOFF_SECS (5s).
        // The test will sleep ~5s on the second iteration; this is acceptable
        // under the "moderate" timeout assigned to this shard.
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(
            matches!(outcome, PollOutcome::Authorized(ref r) if r.login == "bob"),
            "expected Authorized(bob)"
        );
    }

    #[tokio::test]
    async fn poll_returns_expired_on_expired_token_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "expired_token" })),
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(matches!(outcome, PollOutcome::Expired), "expected Expired");
    }

    #[tokio::test]
    async fn poll_returns_denied_on_access_denied_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "access_denied" })),
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(matches!(outcome, PollOutcome::Denied), "expected Denied");
    }

    #[tokio::test]
    async fn poll_recovers_from_server_error_and_succeeds() {
        let server = MockServer::start().await;

        // First call: 500 (transient server error).
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call: success.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "gho_tok_after_5xx",
                    "token_type": "bearer"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("carol", "repo, project"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(
            matches!(outcome, PollOutcome::Authorized(ref r) if r.login == "carol"),
            "expected Authorized(carol)"
        );
    }

    #[tokio::test]
    async fn poll_returns_error_for_unexpected_error_code() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "error": "incorrect_device_code"
                })),
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, no_cancel()).await;

        assert!(matches!(outcome, PollOutcome::Error(_)), "expected Error");
    }

    #[tokio::test]
    async fn poll_returns_cancelled_when_cancel_pre_set() {
        let server = MockServer::start().await;
        // No mock needed — the cancel check runs before the first HTTP call.

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (_cancel_tx, cancel_rx) = watch::channel(true); // pre-cancelled
        let outcome =
            flow.poll_for_token("dc-abc", 0, unix_now() + 900, cancel_rx).await;

        assert!(matches!(outcome, PollOutcome::Cancelled), "expected Cancelled");
    }

    #[tokio::test]
    async fn poll_returns_cancelled_via_cancel_signal_mid_loop() {
        let server = MockServer::start().await;

        // Return authorization_pending on every call.
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "authorization_pending" })),
            )
            .mount(&server)
            .await;

        let flow = Arc::new(DeviceFlow::new(config_for(&server), test_client()));
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let flow2 = Arc::clone(&flow);
        let handle = tokio::spawn(async move {
            flow2
                .poll_for_token("dc-abc", 0, unix_now() + 900, cancel_rx)
                .await
        });

        // Yield to let the spawned task run at least one poll iteration.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        cancel_tx.send(true).unwrap();

        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, PollOutcome::Cancelled), "expected Cancelled");
    }

    // ── validate_token tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn validate_token_captures_login_and_scopes() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("dave", "repo, project, read:org"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let record = flow.validate_token("gho_some_token").await.unwrap();

        assert_eq!(record.login, "dave");
        assert_eq!(record.token, "gho_some_token");
        assert_eq!(
            record.granted_scopes,
            vec!["repo", "project", "read:org"]
        );
    }

    #[tokio::test]
    async fn validate_token_handles_missing_scopes_header() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "login": "eve", "id": 5 })),
                // No X-OAuth-Scopes header.
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let record = flow.validate_token("gho_no_scope_token").await.unwrap();

        assert_eq!(record.login, "eve");
        assert!(record.granted_scopes.is_empty());
    }

    // ── probe_org_state tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn probe_org_state_returns_ok_on_200() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "login": "spinyfin" })),
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let state = flow.probe_org_state("gho_tok", Some("spinyfin")).await;

        assert!(matches!(state, OrgAuthState::Ok));
    }

    #[tokio::test]
    async fn probe_org_state_returns_needs_org_approval_on_403_without_sso() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let state = flow.probe_org_state("gho_tok", Some("spinyfin")).await;

        assert!(
            matches!(state, OrgAuthState::NeedsOrgApproval { ref request_url } if request_url.contains("spinyfin")),
            "expected NeedsOrgApproval"
        );
    }

    #[tokio::test]
    async fn probe_org_state_returns_needs_sso_on_403_with_sso_header() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(
                ResponseTemplate::new(403)
                    .append_header(
                        "X-GitHub-SSO",
                        "required; url=https://github.com/orgs/spinyfin/sso?token=abc",
                    ),
            )
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let state = flow.probe_org_state("gho_tok", Some("spinyfin")).await;

        assert!(
            matches!(state, OrgAuthState::NeedsSso { ref sso_url } if sso_url.contains("sso")),
            "expected NeedsSso"
        );
    }

    #[tokio::test]
    async fn probe_org_state_returns_unknown_when_org_is_none() {
        let server = MockServer::start().await;
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let state = flow.probe_org_state("gho_tok", None).await;
        assert!(matches!(state, OrgAuthState::Unknown));
    }

    // ── GitHubAuthState::to_dto tests ────────────────────────────────────────

    #[test]
    fn to_dto_maps_disconnected() {
        assert!(matches!(
            GitHubAuthState::Disconnected.to_dto(),
            GitHubAuthStateDto::Disconnected
        ));
    }

    #[test]
    fn to_dto_maps_requesting_code() {
        assert!(matches!(
            GitHubAuthState::RequestingCode.to_dto(),
            GitHubAuthStateDto::RequestingCode
        ));
    }

    #[test]
    fn to_dto_maps_pending_user_auth() {
        let state = GitHubAuthState::PendingUserAuth {
            device_code: "secret".to_owned(),
            user_code: "ABCD-EFGH".to_owned(),
            verification_uri: "https://github.com/login/device".to_owned(),
            verification_uri_complete: None,
            expires_at: 9999,
            interval_secs: 5,
        };
        let dto = state.to_dto();
        match dto {
            GitHubAuthStateDto::PendingUserAuth {
                user_code,
                verification_uri,
                verification_uri_complete,
                expires_at,
                interval_seconds,
            } => {
                assert_eq!(user_code, "ABCD-EFGH");
                assert_eq!(verification_uri, "https://github.com/login/device");
                assert_eq!(verification_uri_complete, None);
                assert_eq!(expires_at, 9999);
                assert_eq!(interval_seconds, 5);
                // Ensure device_code does NOT appear in the DTO.
            }
            other => panic!("unexpected dto variant: {other:?}"),
        }
    }

    #[test]
    fn to_dto_maps_authorized() {
        let state = GitHubAuthState::Authorized {
            record: TokenRecord {
                token: "secret-token".to_owned(),
                login: "alice".to_owned(),
                granted_scopes: vec!["repo".to_owned(), "project".to_owned()],
                obtained_at: 0,
            },
            org_state: OrgAuthState::Ok,
        };
        let dto = state.to_dto();
        match dto {
            GitHubAuthStateDto::Authorized { login, granted_scopes, org_state } => {
                assert_eq!(login, "alice");
                assert_eq!(granted_scopes, vec!["repo", "project"]);
                assert!(matches!(org_state, OrgAuthState::Ok));
                // Ensure token does NOT appear in the DTO.
            }
            other => panic!("unexpected dto variant: {other:?}"),
        }
    }

    #[test]
    fn to_dto_does_not_expose_device_code() {
        let state = GitHubAuthState::PendingUserAuth {
            device_code: "super-secret-device-code".to_owned(),
            user_code: "WXYZ-1234".to_owned(),
            verification_uri: "https://github.com/login/device".to_owned(),
            verification_uri_complete: None,
            expires_at: 0,
            interval_secs: 5,
        };
        let dto_json = serde_json::to_string(&state.to_dto()).unwrap();
        assert!(
            !dto_json.contains("super-secret-device-code"),
            "device_code must not appear in the DTO: {dto_json}"
        );
    }

    #[test]
    fn to_dto_does_not_expose_token() {
        let state = GitHubAuthState::Authorized {
            record: TokenRecord {
                token: "gho_super_secret_token".to_owned(),
                login: "frank".to_owned(),
                granted_scopes: vec![],
                obtained_at: 0,
            },
            org_state: OrgAuthState::Ok,
        };
        let dto_json = serde_json::to_string(&state.to_dto()).unwrap();
        assert!(
            !dto_json.contains("gho_super_secret_token"),
            "token must not appear in the DTO: {dto_json}"
        );
    }

    // ── GitHubAuthController tests ───────────────────────────────────────────

    #[tokio::test]
    async fn controller_starts_disconnected() {
        let server = MockServer::start().await;
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::new(flow);
        assert!(matches!(ctrl.current_state(), GitHubAuthState::Disconnected));
    }

    #[tokio::test]
    async fn controller_start_flow_transitions_to_authorized() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_code": "dc-test",
                    "user_code": "TEST-CODE",
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900,
                    "interval": 0
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "gho_ctrl_test",
                    "token_type": "bearer"
                })),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock("grace", "repo, project"))
            .mount(&server)
            .await;

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, mut rx) = GitHubAuthController::new(flow);

        ctrl.start_flow().await;

        // Wait until the state is no longer RequestingCode/PendingUserAuth.
        while matches!(
            *rx.borrow(),
            GitHubAuthState::Disconnected
                | GitHubAuthState::RequestingCode
                | GitHubAuthState::PendingUserAuth { .. }
        ) {
            rx.changed().await.unwrap();
        }

        let state = ctrl.current_state();
        assert!(
            matches!(state, GitHubAuthState::Authorized { ref record, .. } if record.login == "grace"),
            "expected Authorized(grace), got {state:?}"
        );
    }

    #[tokio::test]
    async fn controller_disconnect_clears_authorized_state() {
        let server = MockServer::start().await;
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::new(flow);

        // Manually inject an Authorized state.
        ctrl.state_tx.send_replace(GitHubAuthState::Authorized {
            record: TokenRecord {
                token: "tok".to_owned(),
                login: "helen".to_owned(),
                granted_scopes: vec![],
                obtained_at: 0,
            },
            org_state: OrgAuthState::Ok,
        });

        ctrl.disconnect().await;

        assert!(matches!(ctrl.current_state(), GitHubAuthState::Disconnected));
    }

    #[tokio::test]
    async fn controller_update_org_state_updates_authorized_sub_state() {
        let server = MockServer::start().await;
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::new(flow);

        ctrl.state_tx.send_replace(GitHubAuthState::Authorized {
            record: TokenRecord {
                token: "tok".to_owned(),
                login: "iris".to_owned(),
                granted_scopes: vec![],
                obtained_at: 0,
            },
            org_state: OrgAuthState::Unknown,
        });

        ctrl.update_org_state(OrgAuthState::Ok);

        let state = ctrl.current_state();
        assert!(
            matches!(
                state,
                GitHubAuthState::Authorized { org_state: OrgAuthState::Ok, .. }
            ),
            "expected OrgAuthState::Ok"
        );
    }
}
