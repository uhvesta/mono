//! Diagnostic snapshot for the live-status pipeline.
//!
//! [`LiveStatusDebugReport`] is the wire shape returned by the
//! `bossctl live-status debug` verb and the matching engine RPC. It
//! exists to make the nine-stage pipeline (hook ingress → transcript
//! persist → manager notify → slot loop → summarizer call → set +
//! broadcast) inspectable in one round-trip, without log-diving.
//!
//! The chore that introduced this surface (`Instrument live_status
//! pipeline end-to-end`) was explicit that we are NOT to propose
//! another speculative fix — the goal is observability. So the report
//! is deliberately verbose: every step that could fail silently in the
//! prior shape now has a named field here, and the JSON form is the
//! contract `bossctl --json` exposes to the user.

use serde::{Deserialize, Serialize};

/// Top-level debug report: engine-level facts plus per-slot detail
/// for every slot the manager currently tracks. Always includes the
/// engine build SHA so a user can confirm at a glance that they are
/// reading output from the binary they think they're running — the
/// chore was triggered partly by suspicion that the engine on the
/// user's machine was stale relative to a recently-merged PR.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveStatusDebugReport {
    /// Short git SHA (or `unknown`) of the engine binary, baked in
    /// at compile time. Compare against the most recent commit on
    /// the branch the engine was rebuilt from to detect a stale
    /// binary.
    pub engine_build_sha: String,
    /// ISO-8601 UTC timestamp the engine binary was built at, baked
    /// in at compile time.
    pub engine_build_time: String,
    /// True iff `ANTHROPIC_API_KEY` was present in the engine's
    /// agent config at startup. The summarizer cannot succeed
    /// without it; the chore calls out that the "no api key" silent-
    /// failure mode used to be indistinguishable from a 429 or a
    /// redaction-strips-everything tick.
    pub anthropic_api_key_present: bool,
    /// Total number of slots the live-status manager currently has
    /// a per-slot task spawned for.
    pub tracked_slot_count: usize,
    /// Total number of slots whose summarizer is disabled by the
    /// per-slot toggle.
    pub disabled_slot_count: usize,
    /// Per-slot detail. Ordered by `slot_id` ascending so the
    /// non-JSON renderer can print the table row-by-row without an
    /// extra sort.
    pub slots: Vec<LiveStatusSlotDebug>,
}

/// Per-slot diagnostic snapshot. Mirrors the engine-side
/// `SlotDebugSnapshot` (in `engine/src/live_status_loop.rs`), with the
/// ISO-8601 timestamps formatted on the engine side so the wire shape
/// is self-describing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveStatusSlotDebug {
    pub slot_id: u8,
    /// True iff the manager currently has a per-slot task running for
    /// `slot_id`. Should normally be true for every slot a worker is
    /// attached to; if a hook event lands and this is false, the
    /// notify falls into the "no per-slot task" warn path.
    pub task_running: bool,
    /// True iff the per-slot summarizer toggle is currently in the
    /// disabled position. Surfaced separately from `task_running` so
    /// "task running but disabled" (the toggle is what's gating the
    /// model call) is distinguishable from "task missing entirely".
    pub disabled: bool,
    /// Last trigger kind received by the per-slot loop:
    /// `stop` / `post_tool_use` / `activity_changed` / `shutdown`.
    /// `None` until the loop has serviced its first trigger.
    pub last_trigger_kind: Option<String>,
    /// ISO-8601 UTC timestamp of `last_trigger_kind`.
    pub last_trigger_at: Option<String>,
    /// Last summarizer outcome tag. One of:
    /// `success` / `no_api_key` / `empty_after_redaction` /
    /// `api_error` / `transport_error` / `post_filter_dropped`.
    /// `None` if the loop has never called the summarizer for this
    /// slot.
    pub last_outcome_tag: Option<String>,
    /// Human-readable detail for the last outcome — first 80 chars of
    /// the summary on success, status + body snippet on api_error,
    /// transport error message on transport_error, etc.
    pub last_outcome_detail: Option<String>,
    /// ISO-8601 UTC timestamp of `last_outcome_tag`.
    pub last_outcome_at: Option<String>,
    /// ISO-8601 UTC timestamp of the most recent `Success` outcome.
    /// May be older than `last_outcome_at` if the summarizer has
    /// transitioned to failing after a streak of successes.
    pub last_success_at: Option<String>,
    /// First 80 chars of the most recent successful summary text.
    pub last_success_text: Option<String>,
    /// Resolved transcript path the loop is tailing. `None` if the
    /// resolver has never returned a path — strong signal that the
    /// `work_runs.transcript_path` column is NULL for this run.
    pub transcript_path: Option<String>,
    /// Bytes of redacted prompt text fed to the most recent
    /// summarizer call. Helpful for telling "transcript empty after
    /// redaction" from "model errored mid-call".
    pub last_redacted_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_round_trips_through_serde() {
        let original = LiveStatusDebugReport {
            engine_build_sha: "abc1234".into(),
            engine_build_time: "2026-05-11T20:00:00Z".into(),
            anthropic_api_key_present: true,
            tracked_slot_count: 2,
            disabled_slot_count: 1,
            slots: vec![LiveStatusSlotDebug {
                slot_id: 1,
                task_running: true,
                disabled: false,
                last_trigger_kind: Some("stop".into()),
                last_trigger_at: Some("2026-05-11T20:00:01Z".into()),
                last_outcome_tag: Some("success".into()),
                last_outcome_detail: Some("running tests".into()),
                last_outcome_at: Some("2026-05-11T20:00:02Z".into()),
                last_success_at: Some("2026-05-11T20:00:02Z".into()),
                last_success_text: Some("running tests".into()),
                transcript_path: Some(
                    "/Users/u/.claude/projects/foo/sess.jsonl".into(),
                ),
                last_redacted_bytes: Some(1234),
            }],
        };
        let text = serde_json::to_string(&original).unwrap();
        let parsed: LiveStatusDebugReport = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn report_json_carries_engine_identity_at_top_level() {
        // The chore explicitly wants the engine build SHA + API-key
        // presence to be top-level so `bossctl live-status debug --json
        // | jq .engine_build_sha` is the obvious one-liner. Pin the
        // key names so a refactor of the struct doesn't silently
        // rename them.
        let report = LiveStatusDebugReport {
            engine_build_sha: "deadbeef".into(),
            engine_build_time: "2026-05-11T20:00:00Z".into(),
            anthropic_api_key_present: false,
            tracked_slot_count: 0,
            disabled_slot_count: 0,
            slots: vec![],
        };
        let text = serde_json::to_string(&report).unwrap();
        assert!(text.contains("\"engine_build_sha\":\"deadbeef\""), "{text}");
        assert!(
            text.contains("\"anthropic_api_key_present\":false"),
            "{text}"
        );
        assert!(text.contains("\"tracked_slot_count\":0"), "{text}");
    }

    #[test]
    fn slot_debug_unset_fields_serialize_as_null() {
        // The verb output should distinguish "no trigger received yet"
        // from "field absent because of a protocol mismatch" — Option
        // fields must always serialize as `null` rather than be skipped.
        let slot = LiveStatusSlotDebug {
            slot_id: 1,
            task_running: false,
            disabled: false,
            last_trigger_kind: None,
            last_trigger_at: None,
            last_outcome_tag: None,
            last_outcome_detail: None,
            last_outcome_at: None,
            last_success_at: None,
            last_success_text: None,
            transcript_path: None,
            last_redacted_bytes: None,
        };
        let text = serde_json::to_string(&slot).unwrap();
        assert!(text.contains("\"last_trigger_kind\":null"), "{text}");
        assert!(text.contains("\"transcript_path\":null"), "{text}");
        assert!(text.contains("\"last_outcome_tag\":null"), "{text}");
    }
}
