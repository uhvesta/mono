//! External issue tracker abstraction.
//!
//! The `ExternalTracker` trait is the seam between Boss's work-item taxonomy
//! and any upstream tracker (GitHub Projects today; Jira, Linear later).
//! All tracker-specific logic lives in a sub-module; the reconciler only
//! touches the types and trait defined here.

pub mod credentials;
pub mod github;
pub mod reconcile;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

// ── Upstream data types ─────────────────────────────────────────────────────

/// Stable upstream identifier, normalised across tracker kinds.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRef {
    /// Tracker discriminator: `"github"`, `"jira"`, etc.
    pub kind: String,
    /// Stable string id used in `work_items.external_ref_canonical_id`.
    /// For GitHub: `"spinyfin/mono#560"`.
    pub canonical_id: String,
    /// Tracker-specific blob stored opaquely and replayed to the impl.
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamItem {
    pub upstream_ref: UpstreamRef,
    pub title: String,
    pub body: String,
    pub status: UpstreamStatus,
    /// Canonical web URL for the issue.
    pub upstream_url: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub pr_associations: Vec<UpstreamPrAssociation>,
    /// Last-modified time as Unix seconds.
    pub updated_at: i64,
    /// Current project-board column name for this item, if available.
    /// Populated by trackers that expose a project-status concept
    /// (GitHub Projects V2 "Status" field). `None` for trackers that
    /// don't have a board column, where the Status field hasn't been
    /// set on the item, or where fetching it is not supported.
    pub project_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamStatus {
    Open,
    Closed { reason: ClosedReason },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClosedReason {
    Completed,
    NotPlanned,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamPrAssociation {
    pub pr_url: String,
    pub merged: bool,
    pub merged_at: Option<i64>,
}

// ── Context ─────────────────────────────────────────────────────────────────

/// Resolved credential handed to every tracker call.
///
/// For the v1 GitHub impl the credential is implicit (the user's existing
/// `gh` login); this type is a placeholder so the trait surface is stable
/// when PAT-in-keychain support lands later.
#[derive(Debug, Clone)]
pub struct TrackerCredential {
    /// Opaque token or marker.  Empty string = "use ambient auth".
    pub token: String,
}

impl TrackerCredential {
    /// Ambient credential: rely on whatever auth the environment provides
    /// (e.g. `gh auth status`).
    pub fn ambient() -> Self {
        Self { token: String::new() }
    }
}

/// Per-call context passed into every `ExternalTracker` method.
#[derive(Debug, Clone)]
pub struct TrackerContext {
    pub product_id: String,
    /// Kind-specific config JSON (the `products.external_tracker_config` value).
    pub config: serde_json::Value,
    /// Resolved credential for this call.
    pub credential: TrackerCredential,
}

// ── Close reason ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum CloseReason {
    /// The work is done (Behavior 5 close-on-merge, or reverse-close).
    Completed,
    /// Explicitly cancelled / won't fix.
    NotPlanned,
}

// ── Error types ──────────────────────────────────────────────────────────────

/// Error returned by `ExternalTracker::validate_config`.
#[derive(Debug, Error)]
#[error("invalid tracker config: {message}")]
pub struct TrackerConfigError {
    pub message: String,
}

impl TrackerConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// Error returned by fallible `ExternalTracker` methods.
#[derive(Debug, Error)]
pub enum TrackerError {
    /// Transient: network failure, `gh` unavailable, 5xx, rate-limit.
    /// The reconciler retries on the next tick.
    #[error("transient error: {0}")]
    Transient(String),

    /// The tracker config is invalid (e.g. project not found, 404 on list).
    /// Surface as an attention item; do not retry until config is fixed.
    #[error("config invalid: {0}")]
    ConfigInvalid(String),

    /// Auth failure (403 or `gh auth status` failure).
    /// Surface as an attention item; do not retry until auth is fixed.
    #[error("auth error: {0}")]
    Auth(String),

    /// Permission denied on a write operation (403 on close_issue).
    /// Surface as an attention item; do not retry.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The target issue was not found (404 on close_issue).
    /// Treated as equivalent to already-closed by the reconciler.
    #[error("not found: {0}")]
    NotFound(String),

    /// The operation is not supported by this tracker.
    /// Reserved for future read-only tracker variants.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, TrackerError>;

// ── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ExternalTracker: Send + Sync {
    /// Stable discriminator matching `products.external_tracker_kind`.
    fn kind(&self) -> &'static str;

    /// Validate a kind-specific config JSON at write time.
    fn validate_config(&self, config: &serde_json::Value) -> std::result::Result<(), TrackerConfigError>;

    /// Fetch every upstream item in this product's scope.
    /// Pagination is the implementation's responsibility.
    async fn fetch_items(&self, ctx: &TrackerContext) -> Result<Vec<UpstreamItem>>;

    /// Fetch a single upstream item by stable id.
    async fn fetch_item(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> Result<Option<UpstreamItem>>;

    /// Close an upstream issue.
    ///
    /// Implementations MUST be idempotent: closing an already-closed issue
    /// is success.  Error classification drives reconciler retry behavior:
    /// `Transient` → retry; `PermissionDenied` → surface attention item;
    /// `NotFound` → treat as already-closed.
    async fn close_issue(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
        reason: CloseReason,
    ) -> Result<()>;

    /// Set the project-board status for an upstream item to the tracker's
    /// configured "in progress" column.
    ///
    /// Called by the reconciler when a linked Boss task enters the `active`
    /// (Doing) state.  The target column name is read from `ctx.config`
    /// (key `"in_progress_column"`, default `"In progress"`).
    ///
    /// Implementations MUST be idempotent: setting the status to a value
    /// it already holds is success.  The default no-op is correct for
    /// trackers that do not have a project-board concept.
    ///
    /// Error classification: same as `close_issue`.
    async fn set_project_status(
        &self,
        _ctx: &TrackerContext,
        _ref_: &UpstreamRef,
    ) -> Result<()> {
        Ok(())
    }

    /// Attach a label to an upstream item.
    ///
    /// Called by the reconciler after importing a fresh upstream item, so
    /// that humans browsing the upstream tracker can see at a glance which
    /// issues Boss is mirroring.  The default no-op is correct for trackers
    /// that have no label concept.
    ///
    /// Implementations MUST be idempotent: attaching an already-present
    /// label is success.
    ///
    /// Error classification: same as `close_issue`.
    async fn add_label(
        &self,
        _ctx: &TrackerContext,
        _ref_: &UpstreamRef,
        _label: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Post a comment on an upstream issue referencing the PR that closed it.
    ///
    /// Called by the reconciler immediately after `close_issue` succeeds, so
    /// that the issue's timeline shows which PR drove the close.
    ///
    /// Implementations MUST be idempotent: if the same `pr_url` is already
    /// present in an existing comment on the issue, do NOT post a duplicate.
    ///
    /// The default no-op is correct for trackers where PR linkage is handled
    /// automatically (e.g. via PR-body `Closes #N` syntax) or that have no
    /// comment concept.
    ///
    /// Error classification: same as `close_issue`.  A failure here is
    /// non-fatal: the issue is already closed; only the linkage comment is
    /// missing.
    async fn post_closing_pr_comment(
        &self,
        _ctx: &TrackerContext,
        _ref_: &UpstreamRef,
        _pr_url: &str,
    ) -> Result<()> {
        Ok(())
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// In-process registry of `ExternalTracker` implementations, keyed by `kind`.
#[derive(Default)]
pub struct TrackerRegistry {
    trackers: HashMap<&'static str, Arc<dyn ExternalTracker>>,
}

/// Error returned when registering or looking up a tracker.
#[derive(Debug, Error, PartialEq)]
pub enum RegistryError {
    #[error("tracker kind '{0}' is already registered")]
    AlreadyRegistered(&'static str),
    #[error("no tracker registered for kind '{0}'")]
    UnknownKind(String),
}

impl TrackerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tracker.  Returns `Err` if the `kind` is already registered.
    pub fn register(
        &mut self,
        tracker: Arc<dyn ExternalTracker>,
    ) -> std::result::Result<(), RegistryError> {
        let kind = tracker.kind();
        if self.trackers.contains_key(kind) {
            return Err(RegistryError::AlreadyRegistered(kind));
        }
        self.trackers.insert(kind, tracker);
        Ok(())
    }

    /// Look up a tracker by kind string.
    pub fn get(&self, kind: &str) -> std::result::Result<Arc<dyn ExternalTracker>, RegistryError> {
        self.trackers
            .get(kind)
            .cloned()
            .ok_or_else(|| RegistryError::UnknownKind(kind.to_owned()))
    }
}

// ── EchoTracker (test fake) ───────────────────────────────────────────────────

/// Fake tracker used in unit tests.  Records calls and echoes back synthetic
/// data; makes no network requests.
pub struct EchoTracker {
    /// Items returned by `fetch_items`.
    pub items: Vec<UpstreamItem>,
}

impl EchoTracker {
    pub fn new(items: Vec<UpstreamItem>) -> Self {
        Self { items }
    }

    pub fn empty() -> Self {
        Self::new(vec![])
    }
}

#[async_trait]
impl ExternalTracker for EchoTracker {
    fn kind(&self) -> &'static str {
        "echo"
    }

    fn validate_config(&self, _config: &serde_json::Value) -> std::result::Result<(), TrackerConfigError> {
        Ok(())
    }

    async fn fetch_items(&self, _ctx: &TrackerContext) -> Result<Vec<UpstreamItem>> {
        Ok(self.items.clone())
    }

    async fn fetch_item(
        &self,
        _ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> Result<Option<UpstreamItem>> {
        Ok(self.items.iter().find(|i| i.upstream_ref == *ref_).cloned())
    }

    async fn close_issue(
        &self,
        _ctx: &TrackerContext,
        _ref_: &UpstreamRef,
        _reason: CloseReason,
    ) -> Result<()> {
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_arc() -> Arc<dyn ExternalTracker> {
        Arc::new(EchoTracker::empty())
    }

    #[test]
    fn register_and_lookup_succeeds() {
        let mut reg = TrackerRegistry::new();
        reg.register(echo_arc()).unwrap();
        let tracker = reg.get("echo").unwrap();
        assert_eq!(tracker.kind(), "echo");
    }

    #[test]
    fn double_register_returns_error() {
        let mut reg = TrackerRegistry::new();
        reg.register(echo_arc()).unwrap();
        let result = reg.register(echo_arc());
        assert_eq!(result.unwrap_err(), RegistryError::AlreadyRegistered("echo"));
    }

    #[test]
    fn unknown_kind_returns_error() {
        let reg = TrackerRegistry::new();
        match reg.get("github") {
            Err(e) => assert_eq!(e, RegistryError::UnknownKind("github".to_owned())),
            Ok(_) => panic!("expected UnknownKind error"),
        }
    }

    #[test]
    fn empty_registry_get_unknown() {
        let reg = TrackerRegistry::new();
        assert!(matches!(reg.get("jira"), Err(RegistryError::UnknownKind(_))));
    }

    #[tokio::test]
    async fn echo_tracker_fetch_items_empty() {
        let tracker = EchoTracker::empty();
        let ctx = TrackerContext {
            product_id: "p1".into(),
            config: serde_json::Value::Null,
            credential: TrackerCredential::ambient(),
        };
        let items = tracker.fetch_items(&ctx).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn echo_tracker_fetch_item_found_and_not_found() {
        let ref_ = UpstreamRef {
            kind: "echo".into(),
            canonical_id: "echo#1".into(),
            raw: serde_json::Value::Null,
        };
        let item = UpstreamItem {
            upstream_ref: ref_.clone(),
            title: "Test issue".into(),
            body: String::new(),
            status: UpstreamStatus::Open,
            upstream_url: "https://example.com/1".into(),
            labels: vec![],
            assignees: vec![],
            pr_associations: vec![],
            updated_at: 0,
            project_status: None,
        };
        let tracker = EchoTracker::new(vec![item]);
        let ctx = TrackerContext {
            product_id: "p1".into(),
            config: serde_json::Value::Null,
            credential: TrackerCredential::ambient(),
        };
        assert!(tracker.fetch_item(&ctx, &ref_).await.unwrap().is_some());

        let missing = UpstreamRef {
            kind: "echo".into(),
            canonical_id: "echo#99".into(),
            raw: serde_json::Value::Null,
        };
        assert!(tracker.fetch_item(&ctx, &missing).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn echo_tracker_close_issue_is_noop() {
        let tracker = EchoTracker::empty();
        let ctx = TrackerContext {
            product_id: "p1".into(),
            config: serde_json::Value::Null,
            credential: TrackerCredential::ambient(),
        };
        let ref_ = UpstreamRef {
            kind: "echo".into(),
            canonical_id: "echo#1".into(),
            raw: serde_json::Value::Null,
        };
        tracker.close_issue(&ctx, &ref_, CloseReason::Completed).await.unwrap();
    }
}
