//! Classification of Claude API errors observed in a worker's
//! transcript, and the bounded-retry policy the engine uses to decide
//! whether to auto-resume a stalled/dead worker or escalate it for
//! human attention.
//!
//! ## Why this lives in the engine
//!
//! A Boss worker is an interactive `claude` session in a libghostty
//! pane (`runner.rs` launches `claude … "$(cat initial-prompt.txt)"`
//! with no `--print`). When claude hits a fatal API error mid-run it
//! prints the error, ends the turn, and returns to its REPL — the
//! process stays **alive but idle**. The events socket reports the
//! turn-ending `Stop` as `Idle`, so the worker looks "done" while
//! actually being wedged. Neither the dead-PID sweep (PID is alive)
//! nor the completion path (no PR, no clean finish) recovers it; a
//! human has to notice. This module is the classification half of the
//! engine-owned reconciler ([`crate::transient_recovery`]) that closes
//! that gap.
//!
//! ## Ground truth
//!
//! The transcript JSONL is the authoritative signal. We only treat a
//! worker as recoverable when the **last meaningful transcript entry**
//! is an API-error message ([`extract_worker_error`]): if the worker
//! emitted any normal assistant/tool/user activity after the error, it
//! recovered on its own and we leave it alone. This avoids trusting a
//! single fragile surface (an `Idle` hook can mean "finished cleanly"
//! or "wedged on an error" — only the transcript disambiguates).
//!
//! ## Classification rules
//!
//! [`classify_claude_error`] maps an error string to one of three
//! classes. The rules are intentionally conservative — when in doubt
//! we do NOT auto-retry:
//!
//! - [`ErrorClass::Transient`] — retryable infrastructure hiccups:
//!   socket closed / connection reset / broken pipe, `overloaded_error`
//!   (HTTP 529), `rate_limit`/HTTP 429, request timeouts, and HTTP 5xx
//!   (`api_error`, 500/502/503/504, "internal server error", "service
//!   unavailable", "bad gateway", "gateway timeout").
//! - [`ErrorClass::Permanent`] — non-retryable; retrying would just
//!   reproduce the failure: `authentication_error`/401,
//!   `permission_error`/403, `invalid_request_error`/400,
//!   `not_found_error`/404, `request_too_large`, `billing_error`,
//!   context-length overflow.
//! - [`ErrorClass::Indeterminate`] — an API error we recognise as an
//!   error but cannot confidently bucket. Treated like Permanent by
//!   the policy (escalate, don't blindly retry).

use std::time::Duration;

use serde_json::Value;

/// How an observed Claude API error should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Retryable infrastructure error — auto-resume is appropriate.
    Transient,
    /// Non-retryable error — auto-resume would just reproduce it.
    Permanent,
    /// Recognised as an API error but not confidently classifiable.
    /// The policy treats this like [`ErrorClass::Permanent`].
    Indeterminate,
}

/// Substrings (matched against a lowercased haystack) that mark a
/// **permanent**, non-retryable failure. Checked before the transient
/// markers so an unambiguous auth/billing/invalid-request error never
/// gets auto-retried just because the message also mentions, say, a
/// status code we'd otherwise read as transient.
const PERMANENT_MARKERS: &[&str] = &[
    "authentication_error",
    "authentication error",
    "invalid api key",
    "invalid x-api-key",
    "x-api-key",
    "could not resolve authentication",
    "permission_error",
    "permission denied",
    "invalid_request_error",
    "invalid request",
    "not_found_error",
    "model not found",
    "request_too_large",
    "request too large",
    "prompt is too long",
    "maximum context length",
    "context window",
    "context_length_exceeded",
    "billing_error",
    "credit balance is too low",
    "quota",
    " 400 ",
    " 401 ",
    " 403 ",
    " 404 ",
    "http 400",
    "http 401",
    "http 403",
    "http 404",
    "error code: 400",
    "error code: 401",
    "error code: 403",
    "error code: 404",
];

/// Substrings (matched against a lowercased haystack) that mark a
/// **transient**, retryable failure.
const TRANSIENT_MARKERS: &[&str] = &[
    // Transport / connection.
    "socket connection was closed",
    "socket hang up",
    "connection was closed",
    "connection closed",
    "connection reset",
    "econnreset",
    "connection error",
    "connection aborted",
    "broken pipe",
    "epipe",
    "network error",
    "unexpected eof",
    "incomplete chunked",
    "stream interrupted",
    "stream error",
    // Structured Anthropic error types.
    "overloaded_error",
    "overloaded",
    "api_error",
    "rate_limit_error",
    "rate_limit",
    "rate limit",
    "too many requests",
    // Timeouts.
    "request timed out",
    "timed out",
    "timeout",
    "etimedout",
    "deadline exceeded",
    // HTTP 5xx / 429 / 529.
    "internal server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    " 429 ",
    " 500 ",
    " 502 ",
    " 503 ",
    " 504 ",
    " 529 ",
    "http 429",
    "http 500",
    "http 502",
    "http 503",
    "http 504",
    "http 529",
    "error code: 429",
    "error code: 500",
    "error code: 502",
    "error code: 503",
    "error code: 504",
    "error code: 529",
];

/// Classify a Claude API error string. See the module docs for the
/// rule set. The match is case-insensitive and substring-based; we
/// pad the haystack with spaces so bare-number markers like `" 500 "`
/// match a code at the very start or end of the string.
pub fn classify_claude_error(text: &str) -> ErrorClass {
    let haystack = format!(" {} ", text.to_lowercase());
    // Permanent wins on overlap: never auto-retry an unambiguous
    // non-retryable failure.
    if PERMANENT_MARKERS.iter().any(|m| haystack.contains(m)) {
        return ErrorClass::Permanent;
    }
    if TRANSIENT_MARKERS.iter().any(|m| haystack.contains(m)) {
        return ErrorClass::Transient;
    }
    ErrorClass::Indeterminate
}

/// Extract the worker-halting API-error text from a transcript, but
/// only when it is the **last meaningful entry** — i.e. the worker did
/// not recover and continue working after it.
///
/// Returns `None` when there is no API error, or when the worker
/// emitted normal activity (assistant text/tool use, a user/tool
/// result) after the most recent API error. `lines` is a slice of
/// parsed transcript JSONL values, oldest-first.
pub fn extract_worker_error(lines: &[Value]) -> Option<String> {
    let mut last_error: Option<(usize, String)> = None;
    let mut last_progress: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(text) = entry_api_error_text(line) {
            last_error = Some((i, text));
        } else if is_progress_entry(line) {
            last_progress = Some(i);
        }
    }
    match last_error {
        Some((error_idx, text))
            if last_progress.is_none_or(|progress_idx| progress_idx < error_idx) =>
        {
            Some(text)
        }
        _ => None,
    }
}

/// If `line` is an API-error transcript entry, return its error text.
///
/// Claude Code records a fatal API error as an assistant entry flagged
/// `isApiErrorMessage: true` whose text content reads `API Error: …`.
/// We also accept any assistant/system entry whose extracted text
/// begins with `api error` (case-insensitive) as a belt-and-braces
/// fallback in case the flag is absent on a given client version.
fn entry_api_error_text(line: &Value) -> Option<String> {
    let entry_type = line.get("type").and_then(Value::as_str).unwrap_or("");
    if entry_type != "assistant" && entry_type != "system" {
        return None;
    }
    let flagged = line
        .get("isApiErrorMessage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = entry_text(line);
    let looks_like_error = text
        .trim_start()
        .to_lowercase()
        .starts_with("api error");
    if flagged || looks_like_error {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            // Flagged but no text we could pull — still a real error.
            Some("API Error (no message text in transcript)".to_owned())
        } else {
            Some(trimmed.to_owned())
        }
    } else {
        None
    }
}

/// True when `line` represents the worker doing real work — used to
/// detect that it recovered after an error. Assistant entries that are
/// themselves API errors are excluded by the caller (they're matched
/// by [`entry_api_error_text`] first).
fn is_progress_entry(line: &Value) -> bool {
    match line.get("type").and_then(Value::as_str) {
        Some("assistant") | Some("user") => !entry_text(line).trim().is_empty()
            || line
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("tool_use") | Some("tool_result")
                        )
                    })
                }),
        _ => false,
    }
}

/// Concatenate the text content of a transcript entry. Handles both
/// the `{message:{content:[{type:"text",text}]}}` shape and the flatter
/// `{message:{text}}` / top-level `{content}` shapes.
fn entry_text(line: &Value) -> String {
    let message = line.get("message").unwrap_or(line);
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for block in blocks {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                parts.push(t);
            }
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    if let Some(t) = message.get("content").and_then(Value::as_str) {
        return t.to_owned();
    }
    if let Some(t) = message.get("text").and_then(Value::as_str) {
        return t.to_owned();
    }
    String::new()
}

/// Why a worker is being escalated instead of auto-resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalateReason {
    /// The error is non-retryable (auth, billing, invalid request, …).
    Permanent,
    /// The error is recognised but not confidently classifiable.
    Indeterminate,
    /// The error is transient but the retry cap has been reached.
    RetriesExhausted,
}

impl EscalateReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EscalateReason::Permanent => "permanent_error",
            EscalateReason::Indeterminate => "unrecognized_error",
            EscalateReason::RetriesExhausted => "retries_exhausted",
        }
    }
}

/// The engine's decision for a stalled/dead worker whose last
/// transcript entry was an API error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Auto-resume on the same workspace. `attempt` is 1-based (the
    /// attempt number this resume represents) and `backoff` is how long
    /// to defer the resume dispatch.
    Resume { attempt: u32, backoff: Duration },
    /// Stop retrying and surface for human attention.
    Escalate { reason: EscalateReason },
}

/// Bounded-retry policy with exponential backoff. The cap and the
/// backoff schedule are the whole "no infinite restart loop"
/// guarantee: after `max_attempts` transient resumes the policy
/// escalates instead of resuming again.
#[derive(Debug, Clone)]
pub struct RecoveryPolicy {
    /// Backoff applied before the Nth resume (1-based). `backoff[i]` is
    /// used for attempt `i+1`. The length of this slice is the retry
    /// cap: once `prior_attempts >= backoff.len()` we escalate.
    backoff: Vec<Duration>,
}

impl Default for RecoveryPolicy {
    /// Three resume attempts with 30s / 2m / 5m exponential-ish
    /// backoff, then escalate.
    fn default() -> Self {
        Self {
            backoff: vec![
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(300),
            ],
        }
    }
}

impl RecoveryPolicy {
    /// Construct a policy from an explicit backoff schedule (used by
    /// tests). The schedule length is the retry cap.
    pub fn with_backoff(backoff: Vec<Duration>) -> Self {
        Self { backoff }
    }

    /// The retry cap — how many transient resumes are allowed before
    /// the policy escalates.
    pub fn max_attempts(&self) -> u32 {
        self.backoff.len() as u32
    }

    /// Decide what to do given the error class and how many transient
    /// resumes have already happened on this work item's chain
    /// (`prior_attempts`, i.e. the dead execution's
    /// `transient_failure_count`).
    pub fn decide(&self, class: ErrorClass, prior_attempts: u32) -> RecoveryDecision {
        match class {
            ErrorClass::Permanent => RecoveryDecision::Escalate {
                reason: EscalateReason::Permanent,
            },
            ErrorClass::Indeterminate => RecoveryDecision::Escalate {
                reason: EscalateReason::Indeterminate,
            },
            ErrorClass::Transient => {
                if (prior_attempts as usize) < self.backoff.len() {
                    RecoveryDecision::Resume {
                        attempt: prior_attempts + 1,
                        backoff: self.backoff[prior_attempts as usize],
                    }
                } else {
                    RecoveryDecision::Escalate {
                        reason: EscalateReason::RetriesExhausted,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── classify_claude_error ────────────────────────────────────────

    #[test]
    fn socket_closed_is_transient() {
        assert_eq!(
            classify_claude_error("API Error: The socket connection was closed unexpectedly."),
            ErrorClass::Transient,
        );
    }

    #[test]
    fn overloaded_and_rate_limit_and_5xx_are_transient() {
        for s in [
            "overloaded_error: Overloaded",
            "Error code: 529 - overloaded_error",
            "rate_limit_error: rate limit exceeded",
            "HTTP 429 Too Many Requests",
            "api_error: internal server error",
            "Error code: 503 - service unavailable",
            "502 Bad Gateway",
            "Request timed out after 600s",
            "connection reset by peer (ECONNRESET)",
            "write EPIPE: broken pipe",
        ] {
            assert_eq!(
                classify_claude_error(s),
                ErrorClass::Transient,
                "expected transient for: {s}",
            );
        }
    }

    #[test]
    fn auth_billing_invalid_are_permanent() {
        for s in [
            "authentication_error: invalid x-api-key",
            "Error code: 401 - authentication_error",
            "permission_error: permission denied",
            "invalid_request_error: messages.0 is invalid",
            "not_found_error: model not found",
            "billing_error: Your credit balance is too low",
            "prompt is too long: 250000 tokens > 200000 maximum",
        ] {
            assert_eq!(
                classify_claude_error(s),
                ErrorClass::Permanent,
                "expected permanent for: {s}",
            );
        }
    }

    #[test]
    fn unknown_text_is_indeterminate() {
        assert_eq!(
            classify_claude_error("API Error: something we have never seen"),
            ErrorClass::Indeterminate,
        );
        assert_eq!(classify_claude_error(""), ErrorClass::Indeterminate);
    }

    #[test]
    fn permanent_wins_over_transient_on_overlap() {
        // A message that mentions both an auth failure and a timeout
        // must NOT be auto-retried.
        assert_eq!(
            classify_claude_error("authentication_error after request timed out"),
            ErrorClass::Permanent,
        );
    }

    // ─── extract_worker_error ─────────────────────────────────────────

    fn assistant_text(text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
    }

    fn api_error_entry(text: &str) -> Value {
        json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
    }

    fn tool_use_entry(name: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "tool_use", "name": name, "input": {}}]}
        })
    }

    #[test]
    fn trailing_api_error_is_extracted() {
        let lines = vec![
            assistant_text("working on it"),
            tool_use_entry("Edit"),
            api_error_entry("API Error: The socket connection was closed unexpectedly."),
        ];
        assert_eq!(
            extract_worker_error(&lines).as_deref(),
            Some("API Error: The socket connection was closed unexpectedly."),
        );
    }

    #[test]
    fn api_error_detected_by_text_prefix_without_flag() {
        let lines = vec![json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "API Error: 500 internal server error"}]}
        })];
        assert_eq!(
            extract_worker_error(&lines).as_deref(),
            Some("API Error: 500 internal server error"),
        );
    }

    #[test]
    fn error_followed_by_progress_yields_none() {
        // Worker hit an error but recovered and kept working — not a
        // stall, so we must NOT try to recover it.
        let lines = vec![
            api_error_entry("API Error: overloaded_error"),
            assistant_text("retrying"),
            tool_use_entry("Bash"),
        ];
        assert_eq!(extract_worker_error(&lines), None);
    }

    #[test]
    fn no_error_yields_none() {
        let lines = vec![assistant_text("all done"), tool_use_entry("Read")];
        assert_eq!(extract_worker_error(&lines), None);
    }

    #[test]
    fn flagged_error_without_text_still_extracts() {
        let lines = vec![json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "message": {"role": "assistant", "content": []}
        })];
        assert!(extract_worker_error(&lines).is_some());
    }

    #[test]
    fn empty_transcript_yields_none() {
        assert_eq!(extract_worker_error(&[]), None);
    }

    // ─── RecoveryPolicy::decide ───────────────────────────────────────

    #[test]
    fn transient_resumes_until_cap_then_escalates() {
        let policy = RecoveryPolicy::default();
        assert_eq!(policy.max_attempts(), 3);

        // prior_attempts 0,1,2 → resume with growing backoff.
        match policy.decide(ErrorClass::Transient, 0) {
            RecoveryDecision::Resume { attempt, backoff } => {
                assert_eq!(attempt, 1);
                assert_eq!(backoff, Duration::from_secs(30));
            }
            other => panic!("expected resume, got {other:?}"),
        }
        match policy.decide(ErrorClass::Transient, 1) {
            RecoveryDecision::Resume { attempt, backoff } => {
                assert_eq!(attempt, 2);
                assert_eq!(backoff, Duration::from_secs(120));
            }
            other => panic!("expected resume, got {other:?}"),
        }
        match policy.decide(ErrorClass::Transient, 2) {
            RecoveryDecision::Resume { attempt, backoff } => {
                assert_eq!(attempt, 3);
                assert_eq!(backoff, Duration::from_secs(300));
            }
            other => panic!("expected resume, got {other:?}"),
        }
        // prior_attempts 3 == cap → escalate, no infinite loop.
        assert_eq!(
            policy.decide(ErrorClass::Transient, 3),
            RecoveryDecision::Escalate {
                reason: EscalateReason::RetriesExhausted
            },
        );
        // And it stays escalated beyond the cap.
        assert_eq!(
            policy.decide(ErrorClass::Transient, 9),
            RecoveryDecision::Escalate {
                reason: EscalateReason::RetriesExhausted
            },
        );
    }

    #[test]
    fn backoff_is_monotonically_non_decreasing() {
        let policy = RecoveryPolicy::default();
        let mut prev = Duration::ZERO;
        for attempts in 0..policy.max_attempts() {
            if let RecoveryDecision::Resume { backoff, .. } =
                policy.decide(ErrorClass::Transient, attempts)
            {
                assert!(backoff >= prev, "backoff must not shrink");
                prev = backoff;
            } else {
                panic!("expected resume under cap");
            }
        }
    }

    #[test]
    fn permanent_and_indeterminate_always_escalate() {
        let policy = RecoveryPolicy::default();
        assert_eq!(
            policy.decide(ErrorClass::Permanent, 0),
            RecoveryDecision::Escalate {
                reason: EscalateReason::Permanent
            },
        );
        assert_eq!(
            policy.decide(ErrorClass::Indeterminate, 0),
            RecoveryDecision::Escalate {
                reason: EscalateReason::Indeterminate
            },
        );
    }
}
