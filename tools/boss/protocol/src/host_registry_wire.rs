use serde::{Deserialize, Serialize};

/// One registered host plus all its current capabilities.
/// Wire type for [`FrontendEvent::HostsList`], [`FrontendEvent::HostResult`],
/// and [`FrontendEvent::HostUpdated`].
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct HostSnapshot {
    /// Short identifier. `"local"` is the built-in host; remote hosts
    /// use whatever name was given to `bossctl hosts add` / `AddHost`.
    pub id: String,
    /// SSH target string (e.g. `user@hostname` or an SSH alias).
    /// `None` for the `local` host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_target: Option<String>,
    /// Maximum concurrent worker slots on this host.
    pub pool_size: i64,
    /// Whether the host will accept new work dispatches.
    pub enabled: bool,
    /// ISO-8601 timestamp of the last successful heartbeat. `None`
    /// when the host has never been seen (newly registered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// Human-readable description of the last error, when the host is
    /// in a degraded state (e.g. wrapper push failed at registration).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_text: Option<String>,
    /// ISO-8601 timestamp of host registration.
    pub created_at: String,
    /// All capabilities on this host (both auto-discovered and
    /// user-tagged), ordered source-then-name.
    pub capabilities: Vec<HostCapabilitySnapshot>,
}

/// One capability on a host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostCapabilitySnapshot {
    pub capability: String,
    /// `"auto"` (engine-discovered) or `"user"` (manually tagged).
    pub source: String,
}
