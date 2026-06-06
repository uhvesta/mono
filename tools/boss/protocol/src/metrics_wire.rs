use serde::{Deserialize, Serialize};

/// In-memory snapshot of one metric (counter or gauge), returned by
/// [`FrontendEvent::MetricsShowLiveResult`]. Values are read directly
/// from the engine's atomics so they are not subject to the 30s
/// flush-staleness window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct MetricLiveEntry {
    pub name: String,
    pub description: String,
    /// `"counter"` or `"gauge"`.
    pub kind: String,
    /// Counter value cast to `i64` (same bit pattern — values above
    /// `i64::MAX` are theoretical). Gauge value is a signed `i64`.
    pub value: i64,
    /// Milliseconds since Unix epoch of the last increment (counter)
    /// or set (gauge). 0 means "never updated since registration".
    pub timestamp_ms: i64,
    /// True when this entry was rehydrated from `state.db` but no
    /// handle in the current binary matches its name.
    pub stale: bool,
}
