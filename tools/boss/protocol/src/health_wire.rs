use serde::{Deserialize, Serialize};

/// Engine-side health snapshot returned by
/// [`FrontendEvent::EngineHealthResult`]. The chore that introduced
/// this surface (#699) was triggered by silent summarization failure
/// when `ANTHROPIC_API_KEY` is missing — the macOS app showed nothing,
/// the user only noticed because live-status sentences never appeared.
///
/// `issues` is the structured list the UI renders. It is intentionally
/// extensible: the chore notes other required config (engine socket
/// path, etc.) "likely also applies", so the shape is "report a list
/// of named problems" rather than a one-off boolean.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineHealthReport {
    /// True iff the engine's agent config had an `ANTHROPIC_API_KEY`
    /// at startup. Surfaced as a top-level bit (rather than only via
    /// the `issues` list) so a CLI consumer doing
    /// `boss engine health --json | jq .anthropic_api_key_present`
    /// gets a single boolean without having to grep through the issues
    /// array.
    pub anthropic_api_key_present: bool,
    /// True when dispatch is globally paused. A paused engine will not
    /// dispatch new executions from any source until explicitly resumed via
    /// `SetDispatchPaused { paused: false }`. Surfaced as a top-level field
    /// (in addition to the `issues` list entry) so CLI consumers can check
    /// it with a simple `jq .dispatch_paused`.
    #[serde(default)]
    pub dispatch_paused: bool,
    /// Issues the UI should render, in display order (highest priority
    /// first). Empty when the engine is healthy.
    pub issues: Vec<EngineHealthIssue>,
}

/// One UI-actionable engine-health issue. Carries pre-rendered title
/// and body strings so the macOS app can show the banner without
/// translating engine state into prose at the call site. The engine
/// owns the wording; the UI owns the styling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineHealthIssue {
    /// Stable lowercase snake_case kind identifier. The UI uses this
    /// as a styling / icon / dismissal-state key. Initial values:
    /// - `missing_anthropic_api_key` — engine started without an
    ///   `ANTHROPIC_API_KEY`; summarizer cannot succeed.
    pub kind: String,
    /// `"error"` (a user-visible feature is broken) or `"warning"`
    /// (a background feature is degraded). The banner styling keys
    /// off this so an error renders in red and a warning in amber.
    pub severity: String,
    /// One-line title rendered inline in the banner.
    pub title: String,
    /// Multi-line body with the remediation steps (e.g. which env var
    /// to set and where to restart). The UI wraps and renders verbatim.
    pub body: String,
}
