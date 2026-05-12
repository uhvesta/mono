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
    /// Runtime fingerprint of the engine binary's on-disk content
    /// (short SHA-256 of `current_exe()` bytes). Survives a bazel
    /// cache hit that doesn't update the file mtime, so an operator
    /// who suspects a stale binary can cross-check this against a
    /// fingerprint computed against the build output they intended
    /// to ship.
    pub engine_binary_fingerprint: String,
    /// ISO-8601 UTC timestamp of when this engine process started.
    /// Cross-check against `engine_build_time` and the merge time of
    /// the fix you expect to be running — a process started before
    /// the merge cannot contain the fix.
    pub engine_process_started_at: String,
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
    /// Engine-wide counters for the hook-event dispatcher. These
    /// answer the question the prior debug surface couldn't:
    /// "did `set_run_transcript_path_if_unset` ever actually get
    /// called, and what did it return?". Without these, a slot's
    /// `last_trigger_kind=post_tool_use` was ambiguous between a
    /// real hook arrival and the per-slot loop's synthetic
    /// 60-second timer firing — both write the same label.
    pub dispatcher_stats: DispatcherStatsReport,
    /// Per-slot detail. Ordered by `slot_id` ascending so the
    /// non-JSON renderer can print the table row-by-row without an
    /// extra sort.
    pub slots: Vec<LiveStatusSlotDebug>,
}

/// Hook-event dispatcher counters. Each counter is monotonic across
/// the lifetime of the engine process. Surfaced at the top level so
/// `bossctl live-status debug --json | jq .dispatcher_stats` is a
/// one-liner — the chore that introduced this surface emphasized
/// per-step visibility for the silent-drop failure modes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatcherStatsReport {
    /// Total hook events received by `dispatch_live_worker_state`
    /// (every hook delivery, regardless of whether it was processed
    /// or dropped).
    pub hook_events_total: u64,
    /// Events dropped at the `run_id` guard — neither `_boss_run_id`
    /// on the payload nor a peer-pid ancestor walk produced a run id.
    /// A non-zero value here means hooks ARE arriving but the engine
    /// can't correlate them.
    pub hook_events_dropped_missing_run_id: u64,
    /// Events whose payload carried a non-empty `transcript_path`
    /// field. Compare against `hook_events_total` to confirm claude
    /// is actually delivering the field on the event types you
    /// expected. The 2026-05-12 incident chasing PR #366 was caused
    /// by this ratio being zero for PostToolUse hooks even though
    /// they were arriving — invisible without this counter.
    pub hook_events_with_transcript_path_in_payload: u64,
    /// Events whose payload did NOT carry a non-empty
    /// `transcript_path` field. If this is non-zero AND
    /// `transcript_path_persist_from_cache` is also non-zero, the
    /// engine's in-memory cache is doing the job that claude's hook
    /// payload should have.
    pub hook_events_without_transcript_path_in_payload: u64,
    /// `set_run_transcript_path_if_unset` calls that actually updated
    /// a `work_runs` row (returned Ok(true)). Each run contributes
    /// at most one to this counter — the first writer wins.
    pub transcript_path_persist_updated: u64,
    /// Persist calls that returned Ok(false) — the row was already
    /// populated for that run. Expected to climb in steady state
    /// (every subsequent hook is a no-op).
    pub transcript_path_persist_noop: u64,
    /// Persist calls that returned Err. Should normally be zero; a
    /// non-zero value means the DB write is failing silently and the
    /// engine logs are the next stop.
    pub transcript_path_persist_err: u64,
    /// Persist calls made using the engine's per-run in-memory
    /// transcript-path cache (because the current hook's payload
    /// didn't carry the field). This is the fix introduced by the
    /// 2026-05-12 follow-up chore — a non-zero value confirms the
    /// cache fallback is doing actual work.
    pub transcript_path_persist_from_cache: u64,
    /// Most recent run id the dispatcher saw any hook event for.
    /// Useful for spot-checking against the `run_id` of the slot
    /// you're investigating.
    pub last_hook_run_id: Option<String>,
    /// Most recent hook event kind the dispatcher processed (any
    /// of `session_start`, `user_prompt_submit`, `pre_tool_use`,
    /// `post_tool_use`, `stop`, `notification`, `session_end`).
    /// Distinct from `LiveStatusSlotDebug::last_trigger_kind` —
    /// that field is written by both real hook fan-outs and the
    /// per-slot loop's synthetic timer, while this is strictly the
    /// last REAL hook the dispatcher handled.
    pub last_hook_kind: Option<String>,
    /// ISO-8601 UTC timestamp of `last_hook_kind`.
    pub last_hook_at: Option<String>,
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
    ///
    /// **WARNING:** this label is set by BOTH real hook fan-outs and
    /// the per-slot loop's synthetic 60-second timer firing. A
    /// `post_tool_use` value is therefore ambiguous between "a real
    /// PostToolUse hook arrived" and "the timer fired while activity
    /// was Working". For the unambiguous answer, read
    /// `last_real_trigger_kind` and the top-level
    /// `dispatcher_stats.last_hook_kind` instead.
    pub last_trigger_kind: Option<String>,
    /// ISO-8601 UTC timestamp of `last_trigger_kind`.
    pub last_trigger_at: Option<String>,
    /// Last trigger that originated from a real hook fan-out (i.e.
    /// `notify()` called by `dispatch_live_worker_state`). Excludes
    /// the synthetic timer-floor firings. `None` means the per-slot
    /// loop has never been notified of a hook event for this slot —
    /// strong signal that the dispatcher dropped them upstream
    /// (e.g., at the `run_id` guard or the slot-mapping lookup).
    pub last_real_trigger_kind: Option<String>,
    /// ISO-8601 UTC timestamp of `last_real_trigger_kind`.
    pub last_real_trigger_at: Option<String>,
    /// ISO-8601 UTC timestamp of the most recent synthetic
    /// timer-floor firing in the per-slot loop. `None` means the
    /// timer floor has not fired for this slot. The pre-2026-05-12
    /// debug shape conflated this with real hook arrivals — keep
    /// them distinct here.
    pub last_synthetic_trigger_at: Option<String>,
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
            engine_binary_fingerprint: "ffeedd00".into(),
            engine_process_started_at: "2026-05-12T06:45:38Z".into(),
            anthropic_api_key_present: true,
            tracked_slot_count: 2,
            disabled_slot_count: 1,
            dispatcher_stats: DispatcherStatsReport {
                hook_events_total: 7,
                hook_events_dropped_missing_run_id: 1,
                hook_events_with_transcript_path_in_payload: 4,
                hook_events_without_transcript_path_in_payload: 2,
                transcript_path_persist_updated: 1,
                transcript_path_persist_noop: 3,
                transcript_path_persist_err: 0,
                transcript_path_persist_from_cache: 2,
                last_hook_run_id: Some("run-z".into()),
                last_hook_kind: Some("post_tool_use".into()),
                last_hook_at: Some("2026-05-12T06:48:43Z".into()),
            },
            slots: vec![LiveStatusSlotDebug {
                slot_id: 1,
                task_running: true,
                disabled: false,
                last_trigger_kind: Some("stop".into()),
                last_trigger_at: Some("2026-05-11T20:00:01Z".into()),
                last_real_trigger_kind: Some("stop".into()),
                last_real_trigger_at: Some("2026-05-11T20:00:01Z".into()),
                last_synthetic_trigger_at: None,
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
            engine_binary_fingerprint: "cafebabe".into(),
            engine_process_started_at: "2026-05-11T20:00:00Z".into(),
            anthropic_api_key_present: false,
            tracked_slot_count: 0,
            disabled_slot_count: 0,
            dispatcher_stats: DispatcherStatsReport::default(),
            slots: vec![],
        };
        let text = serde_json::to_string(&report).unwrap();
        assert!(text.contains("\"engine_build_sha\":\"deadbeef\""), "{text}");
        assert!(
            text.contains("\"engine_binary_fingerprint\":\"cafebabe\""),
            "{text}"
        );
        assert!(
            text.contains("\"engine_process_started_at\":\"2026-05-11T20:00:00Z\""),
            "{text}"
        );
        assert!(
            text.contains("\"anthropic_api_key_present\":false"),
            "{text}"
        );
        assert!(text.contains("\"tracked_slot_count\":0"), "{text}");
        assert!(text.contains("\"dispatcher_stats\""), "{text}");
        assert!(text.contains("\"hook_events_total\":0"), "{text}");
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
            last_real_trigger_kind: None,
            last_real_trigger_at: None,
            last_synthetic_trigger_at: None,
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
        assert!(text.contains("\"last_real_trigger_kind\":null"), "{text}");
        assert!(text.contains("\"last_synthetic_trigger_at\":null"), "{text}");
        assert!(text.contains("\"transcript_path\":null"), "{text}");
        assert!(text.contains("\"last_outcome_tag\":null"), "{text}");
    }
}
