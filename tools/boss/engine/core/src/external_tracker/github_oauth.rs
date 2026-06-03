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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::time::sleep;

use boss_protocol::{GitHubAuthStateDto, OrgAuthState};

use crate::external_tracker::github::GitHubConfig;
use crate::work::WorkDb;

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

#[derive(Debug, Deserialize, bon::Builder)]
#[builder(on(String, into))]
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
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
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
            if let Some(sso_header) = response.headers().get("X-GitHub-SSO")
                && let Ok(s) = sso_header.to_str()
                    && s.contains("required") {
                        // Header format: "required; url=https://github.com/orgs/foo/sso?..."
                        if let Some(url_part) = s.split("url=").nth(1) {
                            return OrgAuthState::NeedsSso {
                                sso_url: url_part.trim().to_owned(),
                            };
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
    /// Durable token store.  `Some` in production (the engine wires the
    /// keychain in); `None` in unit tests that only exercise the in-memory
    /// state machine.  When present, the controller persists the captured
    /// token on `Authorized` and deletes it on `disconnect`.
    store: Option<Arc<KeychainTokenStore>>,
}

impl GitHubAuthController {
    /// Create a new controller with no durable store.  Returns the controller
    /// and an initial state receiver that the caller can watch for state-change
    /// notifications.  Used by tests that only exercise the state machine.
    pub fn new(flow: DeviceFlow) -> (Self, watch::Receiver<GitHubAuthState>) {
        Self::build(flow, None)
    }

    /// Create a controller backed by a durable [`KeychainTokenStore`].  The
    /// captured token is persisted on `Authorized` (before the state is
    /// broadcast, so a sync that fires immediately afterward finds it) and
    /// deleted on `disconnect`.  This is the production constructor T-4 uses
    /// from `app.rs`.
    pub fn with_store(
        flow: DeviceFlow,
        store: Arc<KeychainTokenStore>,
    ) -> (Self, watch::Receiver<GitHubAuthState>) {
        Self::build(flow, Some(store))
    }

    fn build(
        flow: DeviceFlow,
        store: Option<Arc<KeychainTokenStore>>,
    ) -> (Self, watch::Receiver<GitHubAuthState>) {
        let (tx, rx) = watch::channel(GitHubAuthState::Disconnected);
        let ctrl = Self {
            state_tx: tx,
            cancel_slot: Arc::new(Mutex::new(None)),
            flow: Arc::new(flow),
            store,
        };
        (ctrl, rx)
    }

    /// Handle to the underlying [`DeviceFlow`].  T-4's orchestrator uses this
    /// to run the org/SSO probe ([`probe_and_record_org_state`]) with the
    /// engine's shared HTTP client rather than standing up a second one.
    pub fn device_flow(&self) -> Arc<DeviceFlow> {
        Arc::clone(&self.flow)
    }

    /// Re-hydrate state from the durable store at engine startup.  If a token
    /// is persisted, transitions to `Authorized { org_state: Unknown }` so the
    /// status surface reflects the connection across engine restarts; the org
    /// probe then runs to resolve `org_state`.  A keychain read error is
    /// logged and treated as "no token" (design §5: keychain unavailable →
    /// fall back, never panic).  Returns `true` if a token was restored.
    pub fn restore_from_store(&self) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        match store.get() {
            Ok(Some(record)) => {
                self.state_tx.send_replace(GitHubAuthState::Authorized {
                    record,
                    org_state: OrgAuthState::Unknown,
                });
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    target: "boss_engine::external_tracker::github_oauth",
                    error = %e,
                    "restore_from_store: keychain read failed; treating as disconnected"
                );
                false
            }
        }
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
        let store = self.store.clone();

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
                    // Persist the captured token to the durable store before
                    // broadcasting `Authorized`, so a reconcile tick that fires
                    // immediately afterward resolves the token via the keychain.
                    // A keychain write failure is logged but does not fail the
                    // flow — the in-memory state still reflects the live token
                    // (design §5: keychain unavailable → fall back, don't abort).
                    if let Some(store) = &store
                        && let Err(e) = store.set(&record) {
                            tracing::error!(
                                target: "boss_engine::external_tracker::github_oauth",
                                error = %e,
                                "failed to persist OAuth token to keychain; \
                                 token held in memory only"
                            );
                        }
                    // Org state is Unknown here; T-4's orchestrator runs the
                    // org/SSO probe and calls `update_org_state` to resolve it.
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
    /// stored token.  The keychain deletion is unconditional and local even if
    /// the network is down (design §5); a delete failure is logged but the
    /// in-memory state still drops to `Disconnected`.
    pub async fn disconnect(&self) {
        self.signal_cancel().await;
        if let Some(store) = &self.store
            && let Err(e) = store.delete() {
                tracing::warn!(
                    target: "boss_engine::external_tracker::github_oauth",
                    error = %e,
                    "disconnect: failed to delete OAuth token from keychain"
                );
            }
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

// ── Org/SSO probe orchestration (T-4) ─────────────────────────────────────────

/// Attention-item kind raised when the OAuth App is not yet approved for a
/// GitHub-bound product's org.
pub(crate) const ATTN_ORG_UNAPPROVED: &str = "github_oauth_org_unapproved";
/// Attention-item kind raised when the stored token needs SAML SSO
/// authorization for a GitHub-bound product's org.
pub(crate) const ATTN_SSO_REQUIRED: &str = "github_oauth_sso_required";

/// Run the org/SSO probe (design §7) for every GitHub-bound product and reflect
/// the outcome as product attention items (design §8), returning the aggregate
/// [`OrgAuthState`] for the single per-host auth state.
///
/// For each product whose `external_tracker_kind == "github"`, the org login is
/// read from its stored [`GitHubConfig`] and probed with the captured token.
/// Probe results are cached per distinct org, so N products sharing one org
/// cost a single probe. Per product the matching attention item is raised (and
/// the other auth-attention kind resolved):
/// - `Ok` → resolve both auth attention kinds.
/// - `NeedsOrgApproval` → raise [`ATTN_ORG_UNAPPROVED`], resolve the SSO one.
/// - `NeedsSso` → raise [`ATTN_SSO_REQUIRED`], resolve the approval one.
/// - `Unknown` → inconclusive (network/parse error): leave items untouched so a
///   transient blip doesn't flap the banner.
///
/// The returned aggregate is the "worst" state across products
/// (`NeedsSso` > `NeedsOrgApproval` > `Ok` > `Unknown`); the orchestrator
/// records it on the controller via `update_org_state`. When no GitHub-bound
/// product exists the result is `Unknown`.
pub(crate) async fn probe_and_record_org_state(
    work_db: &WorkDb,
    flow: &DeviceFlow,
    token: &str,
) -> OrgAuthState {
    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "boss_engine::external_tracker::github_oauth",
                error = %e,
                "probe_and_record_org_state: list_products failed"
            );
            return OrgAuthState::Unknown;
        }
    };

    let mut per_org: HashMap<String, OrgAuthState> = HashMap::new();
    let mut aggregate = OrgAuthState::Unknown;
    let mut probed_any = false;

    for product in &products {
        let org = match (
            product.external_tracker_kind.as_deref(),
            product.external_tracker_config.as_ref(),
        ) {
            (Some("github"), Some(config)) => {
                match serde_json::from_value::<GitHubConfig>(config.clone()) {
                    Ok(cfg) => cfg.org,
                    Err(e) => {
                        tracing::warn!(
                            target: "boss_engine::external_tracker::github_oauth",
                            product_id = %product.id, error = %e,
                            "probe_and_record_org_state: invalid GitHub config; skipping product"
                        );
                        continue;
                    }
                }
            }
            _ => continue,
        };

        let state = match per_org.get(&org) {
            Some(s) => s.clone(),
            None => {
                let s = flow.probe_org_state(token, Some(org.as_str())).await;
                per_org.insert(org.clone(), s.clone());
                s
            }
        };

        probed_any = true;
        apply_org_attention(work_db, &product.id, &state);
        aggregate = merge_org_state(aggregate, state);
    }

    if !probed_any {
        return OrgAuthState::Unknown;
    }
    aggregate
}

/// Raise the attention item matching `state` on `product_id` and resolve the
/// opposing auth-attention kind. Idempotent: `upsert_external_tracker_attention`
/// is a no-op when an open item of the same kind already exists, so this can
/// run every probe tick without piling up rows.
fn apply_org_attention(work_db: &WorkDb, product_id: &str, state: &OrgAuthState) {
    let resolve = |kind: &str| {
        if let Err(e) = work_db.resolve_external_tracker_attention(product_id, kind) {
            tracing::warn!(
                target: "boss_engine::external_tracker::github_oauth",
                %product_id, %kind, error = %e,
                "resolve_external_tracker_attention (github oauth) failed"
            );
        }
    };
    let raise = |kind: &str, title: &str, body: &str| {
        if let Err(e) = work_db.upsert_external_tracker_attention(product_id, kind, title, body) {
            tracing::warn!(
                target: "boss_engine::external_tracker::github_oauth",
                %product_id, %kind, error = %e,
                "upsert_external_tracker_attention (github oauth) failed"
            );
        }
    };

    match state {
        OrgAuthState::Ok => {
            resolve(ATTN_ORG_UNAPPROVED);
            resolve(ATTN_SSO_REQUIRED);
        }
        OrgAuthState::NeedsOrgApproval { request_url } => {
            let body = format!(
                "Boss is connected to GitHub, but the Boss OAuth App is not yet approved \
                 for this product's organization, so issue sync cannot read its private \
                 issues.\n\nAn organization owner must approve the app at:\n\n{request_url}\n\n\
                 Sync recovers automatically once approval is granted."
            );
            raise(
                ATTN_ORG_UNAPPROVED,
                "GitHub OAuth App not approved for this organization",
                &body,
            );
            resolve(ATTN_SSO_REQUIRED);
        }
        OrgAuthState::NeedsSso { sso_url } => {
            let body = format!(
                "Boss is connected to GitHub, but the stored token needs SAML SSO \
                 authorization for this product's organization before issue sync can \
                 read its private issues.\n\nAuthorize the token via SSO at:\n\n{sso_url}\n\n\
                 Sync recovers automatically once the token is SSO-authorized."
            );
            raise(
                ATTN_SSO_REQUIRED,
                "GitHub token needs SAML SSO authorization",
                &body,
            );
            resolve(ATTN_ORG_UNAPPROVED);
        }
        OrgAuthState::Unknown => {
            // Inconclusive (network error / no org binding). Leave any existing
            // items as-is; the next probe (sync 403 or a Re-check) reclassifies.
        }
    }
}

/// Aggregate two org states into the "worst" for the single per-host auth
/// state: `NeedsSso` > `NeedsOrgApproval` > `Ok` > `Unknown`. A transient
/// `Unknown` from one org never downgrades an `Ok` reached for another.
fn merge_org_state(acc: OrgAuthState, next: OrgAuthState) -> OrgAuthState {
    fn rank(s: &OrgAuthState) -> u8 {
        match s {
            OrgAuthState::Unknown => 0,
            OrgAuthState::Ok => 1,
            OrgAuthState::NeedsOrgApproval { .. } => 2,
            OrgAuthState::NeedsSso { .. } => 3,
        }
    }
    if rank(&next) >= rank(&acc) { next } else { acc }
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

/// Production backend on non-macOS: delegates to the OS credential store via
/// `keyring::Entry`.
#[cfg(not(target_os = "macos"))]
struct KeyringBackend;

#[cfg(not(target_os = "macos"))]
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

/// macOS production backends.
///
/// On release builds (Developer ID-signed with `keychain-access-groups`
/// entitlement), [`DataProtectionKeychainBackend`] stores the token in the
/// data-protection keychain using entitlement-based ACLs rather than
/// per-binary ACLs.  This means a new (re-signed) build of the engine can
/// read the token without triggering a macOS keychain prompt.
///
/// On dev builds (ad-hoc signed, no `keychain-access-groups` entitlement),
/// [`FileBackend`] stores the token in a 0600-mode file under the Boss
/// Application Support directory — the same fallback strategy as
/// `APIKeyStore` in the Swift app.
///
/// # Why three prompts on the old code path
///
/// The old `keyring` backend used `SecKeychainFindGenericPassword` (the
/// *legacy* macOS keychain).  Legacy keychain items carry a trusted-application
/// ACL that records the code-signing identity of each binary that is allowed
/// to access the item.  A new (re-signed) binary is not in that ACL, so
/// macOS shows a prompt for every distinct keychain access from the new
/// binary.  At engine startup there are three such accesses:
///
/// 1. `GitHubAuthController::restore_from_store()` reads the stored token.
/// 2. The `KeychainOAuthResolver` reads the same token when the issue-sync
///    tracker resolves credentials for its first sync cycle.
/// 3. A third read happens when the org-auth probe runs after restoring state.
///
/// The data-protection keychain (via `kSecUseDataProtectionKeychain = true`)
/// enforces access by entitlement rather than binary identity: any binary
/// signed with the same `keychain-access-groups` entitlement can access the
/// item without a user prompt, even after a re-sign.
#[cfg(target_os = "macos")]
mod macos_backends {
    use super::{KeystoreBackend, TokenStoreError, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};

    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::string::{
        CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8,
    };
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password_options, generic_password,
        set_generic_password_options,
    };
    use security_framework_sys::access_control::kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly;
    use security_framework_sys::base::errSecItemNotFound;
    use std::ffi::CString;
    use std::path::PathBuf;

    // `kSecAttrAccessible` is not re-exported by `security-framework-sys`, but
    // it is a plain `extern` symbol in Security.framework.
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecAttrAccessible: CFStringRef;
    }

    // `SecTask*` APIs for checking the `keychain-access-groups` entitlement.
    // These functions are in Security.framework but not wrapped by any crate
    // we use.
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecTaskCreateFromSelf(error: *const std::ffi::c_void) -> CFTypeRef;
        fn SecTaskCopyValueForEntitlement(
            task: CFTypeRef,
            entitlement: CFStringRef,
            error: *mut std::ffi::c_void,
        ) -> CFTypeRef;
    }

    /// Returns `true` when the `keychain-access-groups` entitlement is present
    /// in the running process (i.e. this is a Developer ID release build).
    ///
    /// Mirrors `APIKeyStore.dataProtectionKeychainAvailable()` in the Swift app.
    pub(super) fn data_protection_keychain_available() -> bool {
        // SAFETY: all pointer ops follow CF ownership rules:
        //   - SecTaskCreateFromSelf returns an owned ref (Create rule) → CFRelease
        //   - CFStringCreateWithCString returns an owned ref → CFRelease
        //   - SecTaskCopyValueForEntitlement returns an owned ref (if non-null) → CFRelease
        unsafe {
            let task = SecTaskCreateFromSelf(std::ptr::null());
            if task.is_null() {
                return false;
            }

            let entitlement = CString::new("keychain-access-groups").unwrap();
            let cf_entitlement = CFStringCreateWithCString(
                std::ptr::null_mut(),
                entitlement.as_ptr(),
                kCFStringEncodingUTF8,
            );
            if cf_entitlement.is_null() {
                CFRelease(task);
                return false;
            }

            let value = SecTaskCopyValueForEntitlement(task, cf_entitlement, std::ptr::null_mut());
            CFRelease(task);
            CFRelease(cf_entitlement as CFTypeRef);

            let present = !value.is_null();
            if present {
                CFRelease(value);
            }
            present
        }
    }

    fn read_options() -> PasswordOptions {
        let mut opts = PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        opts.use_protected_keychain();
        opts
    }

    fn write_options() -> PasswordOptions {
        let mut opts = read_options();
        // Add kSecAttrAccessible = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly so
        // that the item can be read by background processes (including this engine)
        // even when the screen is locked.  We use the deprecated `query` field because
        // `PasswordOptions` has no public setter for this attribute.
        #[allow(deprecated)]
        opts.query.push((
            // SAFETY: kSecAttrAccessible is a permanent static string in Security.framework.
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccessible) },
            // SAFETY: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly is a permanent
            // static string in Security.framework; cast from CFStringRef to CFTypeRef is valid.
            unsafe {
                CFString::wrap_under_get_rule(
                    kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as CFStringRef,
                )
            }
            .into_CFType(),
        ));
        opts
    }

    /// Converts a `security_framework` error to `TokenStoreError` by wrapping
    /// it as a `keyring` platform failure.
    fn sec_err(e: security_framework::base::Error) -> TokenStoreError {
        TokenStoreError::Keychain(keyring::Error::PlatformFailure(Box::new(e)))
    }

    // ── DataProtectionKeychainBackend ──────────────────────────────────────────

    /// Stores the OAuth token in the macOS Data Protection Keychain.
    ///
    /// Requires the `keychain-access-groups` entitlement (present in Developer
    /// ID release builds via `engine.entitlements`).
    pub(super) struct DataProtectionKeychainBackend;

    impl KeystoreBackend for DataProtectionKeychainBackend {
        fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
            match generic_password(read_options()) {
                Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
                Err(e) if e.code() == errSecItemNotFound => Ok(None),
                Err(e) => Err(sec_err(e)),
            }
        }

        fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
            set_generic_password_options(value.as_bytes(), write_options()).map_err(sec_err)
        }

        fn delete_raw(&self) -> Result<(), TokenStoreError> {
            match delete_generic_password_options(read_options()) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == errSecItemNotFound => Ok(()),
                Err(e) => Err(sec_err(e)),
            }
        }
    }

    // ── FileBackend ────────────────────────────────────────────────────────────

    /// Stores the OAuth token as a 0600-mode JSON file.
    ///
    /// Used as a fallback on ad-hoc dev builds that lack the
    /// `keychain-access-groups` entitlement needed to access the Data
    /// Protection Keychain.
    pub(super) struct FileBackend {
        path: PathBuf,
    }

    impl FileBackend {
        pub(super) fn new() -> Self {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            Self {
                path: PathBuf::from(home)
                    .join("Library/Application Support/Boss")
                    .join("github-oauth-token"),
            }
        }
    }

    fn io_err(e: std::io::Error) -> TokenStoreError {
        TokenStoreError::Keychain(keyring::Error::PlatformFailure(Box::new(e)))
    }

    impl KeystoreBackend for FileBackend {
        fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
            match std::fs::read_to_string(&self.path) {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io_err(e)),
            }
        }

        fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)
                .map_err(io_err)?;
            file.write_all(value.as_bytes()).map_err(io_err)
        }

        fn delete_raw(&self) -> Result<(), TokenStoreError> {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(e)),
            }
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

impl Default for KeychainTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainTokenStore {
    /// Creates a store backed by the platform's native credential store.
    ///
    /// On macOS, selects between the data-protection keychain (release builds
    /// with `keychain-access-groups` entitlement) and a file-based fallback
    /// (ad-hoc dev builds without the entitlement).  On other platforms,
    /// delegates to `keyring`.
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            if macos_backends::data_protection_keychain_available() {
                tracing::debug!(
                    target: "boss_engine::external_tracker::github_oauth",
                    "github token store: data-protection keychain (release build)"
                );
                Self { backend: Box::new(macos_backends::DataProtectionKeychainBackend) }
            } else {
                tracing::debug!(
                    target: "boss_engine::external_tracker::github_oauth",
                    "github token store: file backend (dev build, no keychain-access-groups entitlement)"
                );
                Self { backend: Box::new(macos_backends::FileBackend::new()) }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self { backend: Box::new(KeyringBackend) }
        }
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

    use std::path::PathBuf;

    use boss_protocol::CreateProductInput;
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

    // ── Keychain wiring (T-4) tests ──────────────────────────────────────────

    async fn mount_full_flow(server: &MockServer, token: &str, login: &str, scopes: &str) {
        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dc-test",
                "user_code": "TEST-CODE",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 0
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": token,
                "token_type": "bearer"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(user_mock(login, scopes))
            .mount(server)
            .await;
    }

    async fn wait_until_authorized(rx: &mut watch::Receiver<GitHubAuthState>) {
        while matches!(
            *rx.borrow(),
            GitHubAuthState::Disconnected
                | GitHubAuthState::RequestingCode
                | GitHubAuthState::PendingUserAuth { .. }
        ) {
            rx.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn controller_with_store_persists_token_on_authorized() {
        let server = MockServer::start().await;
        mount_full_flow(&server, "gho_persist", "grace", "repo, project").await;

        let store = Arc::new(KeychainTokenStore::with_backend(FakeStore::empty()));
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, mut rx) = GitHubAuthController::with_store(flow, Arc::clone(&store));

        ctrl.start_flow().await;
        wait_until_authorized(&mut rx).await;

        let persisted = store.get().unwrap().expect("token should be persisted");
        assert_eq!(persisted.login, "grace");
        assert_eq!(persisted.token, "gho_persist");
    }

    #[tokio::test]
    async fn controller_with_store_deletes_token_on_disconnect() {
        let server = MockServer::start().await;
        let store =
            Arc::new(KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record())));
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::with_store(flow, Arc::clone(&store));

        assert!(store.get().unwrap().is_some());
        ctrl.disconnect().await;

        assert!(
            store.get().unwrap().is_none(),
            "disconnect must clear the keychain item"
        );
        assert!(matches!(ctrl.current_state(), GitHubAuthState::Disconnected));
    }

    #[tokio::test]
    async fn controller_restore_from_store_rehydrates_authorized() {
        let server = MockServer::start().await;
        let store =
            Arc::new(KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record())));
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::with_store(flow, store);

        assert!(matches!(ctrl.current_state(), GitHubAuthState::Disconnected));
        assert!(ctrl.restore_from_store(), "should report a restored token");

        assert!(
            matches!(
                ctrl.current_state(),
                GitHubAuthState::Authorized {
                    ref record,
                    org_state: OrgAuthState::Unknown
                } if record.login == "octocat"
            ),
            "expected restored Authorized(octocat) with Unknown org_state"
        );
    }

    #[tokio::test]
    async fn controller_restore_from_store_noop_when_empty() {
        let server = MockServer::start().await;
        let store = Arc::new(KeychainTokenStore::with_backend(FakeStore::empty()));
        let flow = DeviceFlow::new(config_for(&server), test_client());
        let (ctrl, _rx) = GitHubAuthController::with_store(flow, store);

        assert!(!ctrl.restore_from_store());
        assert!(matches!(ctrl.current_state(), GitHubAuthState::Disconnected));
    }

    // ── Org/SSO probe orchestration (T-4) tests ──────────────────────────────

    fn github_product_db(org: &str) -> (WorkDb, String) {
        let db = WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb");
        let product = db
            .create_product(CreateProductInput {
                name: "Test Product".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .expect("create product");
        let config = serde_json::json!({
            "org": org,
            "repo": "mono",
            "project_number": 1
        });
        db.set_product_external_tracker(&product.id, Some("github"), Some(&config), false)
            .expect("set external tracker");
        (db, product.id)
    }

    fn open_attn_kinds(db: &WorkDb, product_id: &str) -> Vec<String> {
        db.list_attention_items_for_work_item(product_id)
            .expect("list attention items")
            .into_iter()
            .filter(|a| a.status == "open")
            .map(|a| a.kind)
            .collect()
    }

    #[tokio::test]
    async fn probe_org_state_raises_org_approval_attention_on_403() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let (db, product_id) = github_product_db("spinyfin");
        let flow = DeviceFlow::new(config_for(&server), test_client());

        let state = probe_and_record_org_state(&db, &flow, "gho_tok").await;

        assert!(
            matches!(state, OrgAuthState::NeedsOrgApproval { .. }),
            "expected NeedsOrgApproval, got {state:?}"
        );
        let kinds = open_attn_kinds(&db, &product_id);
        assert!(
            kinds.contains(&ATTN_ORG_UNAPPROVED.to_owned()),
            "expected org-unapproved attention item, got {kinds:?}"
        );
        assert!(!kinds.contains(&ATTN_SSO_REQUIRED.to_owned()));
    }

    #[tokio::test]
    async fn probe_org_state_raises_sso_attention_on_sso_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(ResponseTemplate::new(403).append_header(
                "X-GitHub-SSO",
                "required; url=https://github.com/orgs/spinyfin/sso?token=abc",
            ))
            .mount(&server)
            .await;

        let (db, product_id) = github_product_db("spinyfin");
        let flow = DeviceFlow::new(config_for(&server), test_client());

        let state = probe_and_record_org_state(&db, &flow, "gho_tok").await;

        assert!(
            matches!(state, OrgAuthState::NeedsSso { .. }),
            "expected NeedsSso, got {state:?}"
        );
        let kinds = open_attn_kinds(&db, &product_id);
        assert!(
            kinds.contains(&ATTN_SSO_REQUIRED.to_owned()),
            "expected sso-required attention item, got {kinds:?}"
        );
        assert!(!kinds.contains(&ATTN_ORG_UNAPPROVED.to_owned()));
    }

    #[tokio::test]
    async fn probe_org_state_ok_resolves_stale_attention() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/spinyfin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "login": "spinyfin" })),
            )
            .mount(&server)
            .await;

        let (db, product_id) = github_product_db("spinyfin");
        // Seed a stale org-approval attention item; a successful probe must
        // resolve it (design §7 "Re-check" recovery).
        db.upsert_external_tracker_attention(&product_id, ATTN_ORG_UNAPPROVED, "stale", "stale")
            .unwrap();
        assert!(open_attn_kinds(&db, &product_id).contains(&ATTN_ORG_UNAPPROVED.to_owned()));

        let flow = DeviceFlow::new(config_for(&server), test_client());
        let state = probe_and_record_org_state(&db, &flow, "gho_tok").await;

        assert!(matches!(state, OrgAuthState::Ok), "expected Ok, got {state:?}");
        assert!(
            open_attn_kinds(&db, &product_id).is_empty(),
            "Ok probe must resolve stale auth attention items"
        );
    }

    #[tokio::test]
    async fn probe_org_state_unknown_without_github_products() {
        let server = MockServer::start().await;
        let db = WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb");
        let flow = DeviceFlow::new(config_for(&server), test_client());

        let state = probe_and_record_org_state(&db, &flow, "gho_tok").await;
        assert!(matches!(state, OrgAuthState::Unknown));
    }

    #[test]
    fn merge_org_state_prefers_worst() {
        let ok = OrgAuthState::Ok;
        let approval = OrgAuthState::NeedsOrgApproval {
            request_url: "u".to_owned(),
        };
        let sso = OrgAuthState::NeedsSso {
            sso_url: "s".to_owned(),
        };
        assert!(matches!(
            merge_org_state(OrgAuthState::Unknown, ok.clone()),
            OrgAuthState::Ok
        ));
        assert!(matches!(
            merge_org_state(ok.clone(), approval.clone()),
            OrgAuthState::NeedsOrgApproval { .. }
        ));
        assert!(matches!(
            merge_org_state(approval, sso),
            OrgAuthState::NeedsSso { .. }
        ));
        // A transient Unknown never downgrades an Ok.
        assert!(matches!(
            merge_org_state(ok, OrgAuthState::Unknown),
            OrgAuthState::Ok
        ));
    }
}
