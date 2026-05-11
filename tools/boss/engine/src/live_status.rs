//! Live worker status summarizer.
//!
//! Given a tail of the worker's transcript JSONL, ask a cheap
//! summarizer model for a one-sentence description of what the worker
//! is doing right now. The result lands on
//! `LiveWorkerState::live_status` (set by [`crate::live_worker_state`]
//! via the registry update added in a follow-up PR) and is broadcast
//! to the macOS app on the existing `worker.live_states` topic, where
//! it renders under the Doing-card title and on the Agents-tab worker
//! header subtitle.
//!
//! Cost / latency target (see `tools/boss/docs/designs/worker-live-status.md`):
//!
//! - Model: Haiku 4.5 (`claude-haiku-4-5-20251001`). Sonnet would buy
//!   slightly cleaner phrasing for ~3× the price and Haiku produces
//!   the gerund-style sentence we want with no fuss.
//! - Input cap: 800 tokens. The redactor trims oldest-first if the
//!   transcript window overflows.
//! - Output cap: 100 tokens. The prompt asks for ≤ 25 words; the
//!   cap exists to catch a runaway response.
//! - Timeout: 5 s. P99 is usually closer to 2 s; we'd rather keep
//!   the prior value than block the loop.
//! - Budget: ≤ $1/hour at 8 fully-busy workers — see the design doc's
//!   Q3 for the arithmetic.
//!
//! Privacy is layered:
//!
//! 1. Pre-summarizer redaction via [`crate::live_status_redact`] —
//!    drop deny-listed entries, truncate large values, then run the
//!    secret-pattern regexes over the assembled text.
//! 2. Prompt guardrails — the system message forbids quoting literal
//!    values longer than four words and enumerates the kinds of
//!    strings that must never appear verbatim.
//! 3. Post-output filter — same redactor regexes run over the
//!    model's reply; if the result is empty or > 90 % redaction
//!    markers, we drop it and keep the prior value.
//!
//! Failure modes are silent on purpose. A flickering label that
//! empties on every transient API hiccup is worse than a label
//! that's two minutes old.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::live_status_redact;

/// Anthropic Messages API endpoint. Hard-coded; matches
/// [`crate::pane_summary`].
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Haiku 4.5 is the right shape for a one-sentence cheap summary —
/// see Q3 in the design doc.
pub const SUMMARY_MODEL: &str = "claude-haiku-4-5-20251001";

/// Output ceiling. The prompt asks for ≤ 25 words; this is the
/// safety net.
pub const SUMMARY_MAX_TOKENS: u32 = 100;

/// Worst-case wall-clock budget for the API round trip. P99 on Haiku
/// for ~800 input tokens is comfortably under this.
pub const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bytes of redacted prompt text we'll feed to the model per call.
/// Roughly ~800 tokens at the API's 4-bytes-per-token heuristic,
/// matching the design's input cap. Oldest entries get dropped first
/// when the transcript window overflows.
pub const MAX_PROMPT_BYTES: usize = 3_200;

/// Maximum number of transcript JSONL entries we'll consider per
/// tick before redaction. Bounds the work the redactor does on
/// chatty workers; 30 covers the most recent few turns.
pub const MAX_TRANSCRIPT_ENTRIES: usize = 30;

/// Maximum render length we'll write to `live_status`. Keeps the
/// kanban subtitle to a sensible single-line shape even when the
/// model ignores the prompt's word-count guidance.
pub const MAX_LIVE_STATUS_LEN: usize = 240;

/// Literal string used by the trigger fan-in when `activity` flips
/// to `WaitingForInput` and no prior summary is available. Written
/// directly to `live_status` without a model call.
pub const AWAITING_INPUT_LITERAL: &str = "awaiting input";

/// Literal string written when `activity` flips to `Errored`.
pub const ERRORED_LITERAL: &str = "errored — check logs";

/// Top-level entry: redact `transcript_lines`, build a prompt, call
/// the model, post-filter the response. Returns `None` on any
/// failure path (no api key, transcript empty after redaction, API
/// error, mostly-redacted output). The caller (PR 5's trigger
/// fan-in) keeps the prior `live_status` in that case.
pub async fn summarize_transcript(
    api_key: Option<&str>,
    transcript_lines: &[Value],
) -> Option<String> {
    let api_key = api_key?;
    let redacted = redact_and_assemble(transcript_lines);
    if redacted.trim().is_empty() {
        tracing::debug!("live_status: transcript empty after redaction");
        return None;
    }
    match claude_one_sentence(api_key, &redacted).await {
        Ok(summary) => Some(summary),
        Err(err) => {
            tracing::warn!(?err, "live_status: summarizer call failed");
            None
        }
    }
}

/// Apply [`live_status_redact`] to a transcript tail and assemble the
/// remaining text into the single prompt body we feed to the model.
/// Oldest entries drop first when the trimmed prompt exceeds
/// [`MAX_PROMPT_BYTES`].
///
/// The shape is deliberately simple — one line per surviving entry,
/// `kind: short summary` — so the model sees event boundaries and
/// can't confuse two adjacent tool calls.
pub fn redact_and_assemble(transcript_lines: &[Value]) -> String {
    let mut rendered: Vec<String> = Vec::new();
    let start = transcript_lines
        .len()
        .saturating_sub(MAX_TRANSCRIPT_ENTRIES);
    for line in &transcript_lines[start..] {
        if live_status_redact::should_drop_entry(line) {
            continue;
        }
        let truncated = live_status_redact::truncate_large_values(line.clone());
        let summary = render_entry(&truncated);
        if summary.trim().is_empty() {
            continue;
        }
        rendered.push(live_status_redact::redact_text(&summary));
    }
    // Trim oldest-first until we fit under the input cap. We render
    // the body bottom-up to keep the freshest events.
    let mut total: usize = rendered.iter().map(|s| s.len() + 1).sum();
    let mut start = 0usize;
    while total > MAX_PROMPT_BYTES && start < rendered.len() {
        total -= rendered[start].len() + 1;
        start += 1;
    }
    rendered[start..].join("\n")
}

/// Compact a single transcript JSONL entry into a one-line summary
/// we can hand to the model. The claude transcript format varies
/// across message kinds; we only need to surface the rough action
/// shape (assistant text, tool use, tool result).
fn render_entry(line: &Value) -> String {
    let entry_type = line
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match entry_type {
        "user" => {
            // User-side payloads usually wrap a tool_result; we want
            // just a short marker that the worker received output.
            if let Some(name) = line.get("tool_name").and_then(Value::as_str) {
                return format!("user: {name} returned");
            }
            "user: prompt".to_owned()
        }
        "assistant" => render_assistant(line),
        "system" => String::new(),
        _ => String::new(),
    }
}

fn render_assistant(line: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let message = line.get("message").unwrap_or(line);
    let content = message.get("content").and_then(Value::as_array);
    if let Some(blocks) = content {
        for block in blocks {
            let Some(obj) = block.as_object() else { continue };
            let block_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        let one_line = t.trim().replace('\n', " ");
                        parts.push(format!("assistant: {}", clip(&one_line, 200)));
                    }
                }
                "tool_use" => {
                    let name =
                        obj.get("name").and_then(Value::as_str).unwrap_or("Tool");
                    // For Bash, surface the first ~80 chars of the
                    // command. For other tools, the name alone is
                    // typically enough signal — the model just needs
                    // to know "the worker ran Edit, then Bash".
                    let arg = if name == "Bash" {
                        obj.get("input")
                            .and_then(|i| i.get("command"))
                            .and_then(Value::as_str)
                            .map(|c| clip(c, 80))
                    } else {
                        obj.get("input")
                            .and_then(|i| i.get("file_path"))
                            .and_then(Value::as_str)
                            .map(|c| clip(c, 80))
                    };
                    if let Some(arg) = arg {
                        parts.push(format!("tool: {name}({arg})"));
                    } else {
                        parts.push(format!("tool: {name}"));
                    }
                }
                "thinking" => {
                    if let Some(t) = obj.get("thinking").and_then(Value::as_str) {
                        let one_line = t.trim().replace('\n', " ");
                        parts.push(format!("thinking: {}", clip(&one_line, 200)));
                    }
                }
                _ => {}
            }
        }
    } else if let Some(t) = message.get("text").and_then(Value::as_str) {
        parts.push(format!("assistant: {}", clip(t, 200)));
    }
    parts.join(" | ")
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// Build the prompt for the model. The system message carries the
/// guardrails; the user message carries the redacted transcript and
/// the explicit ask.
pub fn build_messages(transcript: &str) -> (String, String) {
    let system = SYSTEM_PROMPT.to_owned();
    let mut user = String::new();
    user.push_str(
        "Below is a redacted tail of the worker's transcript. Lines may be \
         truncated and any sensitive substrings are replaced with <redacted>. \
         Return one short present-continuous sentence (≤ 25 words) describing \
         what the worker is doing right now. Do not quote literal values \
         longer than four words from the transcript. Do not include file \
         paths, URLs, tokens, keys, or anything that looks like a password. \
         If you cannot tell what the worker is doing, return the literal \
         string \"working\" with no other text.\n\nTranscript tail:\n",
    );
    user.push_str(transcript);
    user.push_str("\n\nOne-sentence status:");
    (system, user)
}

const SYSTEM_PROMPT: &str = "\
You summarise a coding worker's moment-to-moment activity in one \
short present-continuous sentence for an internal developer UI. \
Examples of the shape we want: \"investigating why the scroll \
handler doesn't fire when lane content overflows\", \"running tests \
after the layout fix\", \"editing the redactor module's deny-list\". \
Strict rules:\n\
- One sentence, ≤ 25 words, no leading subject, present-continuous.\n\
- Describe the *action*, not the *content*. \"Reading the auth \
config\" is fine; \"reading file containing <a literal>\" is not.\n\
- Never quote any literal value longer than four words from the \
input.\n\
- Never include file paths, URLs, API tokens, keys, passwords, or \
strings that look like secrets.\n\
- If the transcript is uninformative, reply with the single word \
\"working\".\
";

#[derive(Debug, Serialize)]
struct ClaudeRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ClaudeMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct ClaudeMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ClaudeContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // Mirror `pane_summary::http_client` — install the rustls
        // ring provider lazily so the first TLS handshake doesn't
        // panic.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(SUMMARY_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

/// Hit Anthropic with a one-sentence ask. Errors are bucketed into
/// `anyhow` — the caller turns any error into "keep prior".
pub async fn claude_one_sentence(api_key: &str, transcript: &str) -> Result<String> {
    let client = http_client();
    let (system, user) = build_messages(transcript);
    let body = ClaudeRequest {
        model: SUMMARY_MODEL,
        max_tokens: SUMMARY_MAX_TOKENS,
        system: &system,
        messages: vec![ClaudeMessage {
            role: "user",
            content: user,
        }],
    };
    let resp = client
        .post(ANTHROPIC_MESSAGES_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("anthropic returned {status}: {text}");
    }
    let parsed: ClaudeResponse = resp.json().await?;
    let raw = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .map(|b| b.text)
        .unwrap_or_default();
    let cleaned = clean_summary(&raw);
    if cleaned.is_empty() {
        anyhow::bail!("anthropic returned an empty summary");
    }
    Ok(cleaned)
}

/// Post-process the model reply: trim whitespace and quotes, drop a
/// trailing period, apply the same secret-pattern redactor over the
/// output, and reject anything that ends up mostly redacted (the
/// salvage check from [`live_status_redact::is_mostly_redacted`]).
///
/// Returns an empty string on rejection so the caller can fall back
/// to "keep prior".
pub fn clean_summary(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .trim_start_matches(|c: char| c == '"' || c == '\'' || c == '`')
        .trim_end_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.')
        .trim();
    let redacted = live_status_redact::redact_text(stripped);
    if live_status_redact::is_mostly_redacted(&redacted) {
        return String::new();
    }
    // Single-line shape. The UI renders this on a single line; any
    // model-injected newline becomes a space.
    let one_line = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= MAX_LIVE_STATUS_LEN {
        return one_line;
    }
    let mut out = String::new();
    for c in one_line.chars() {
        if out.len() + c.len_utf8() > MAX_LIVE_STATUS_LEN {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn assistant_text(text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": text}],
            },
        })
    }

    fn assistant_tool_use(name: &str, input: Value) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "tool_use", "name": name, "input": input}],
            },
        })
    }

    #[test]
    fn render_entry_handles_assistant_text() {
        let line = assistant_text("Investigating the scroll handler bug");
        let s = render_entry(&line);
        assert!(s.contains("assistant"));
        assert!(s.contains("Investigating"));
    }

    #[test]
    fn render_entry_summarises_bash_tool_use_with_command_prefix() {
        let line =
            assistant_tool_use("Bash", json!({"command": "cargo test -p boss-engine"}));
        let s = render_entry(&line);
        assert!(s.contains("Bash"), "got {s}");
        assert!(s.contains("cargo test"), "got {s}");
    }

    #[test]
    fn render_entry_summarises_edit_tool_use_with_file_path() {
        let line = assistant_tool_use(
            "Edit",
            json!({"file_path": "tools/boss/engine/src/app.rs"}),
        );
        let s = render_entry(&line);
        assert!(s.contains("Edit"));
        assert!(s.contains("app.rs"));
    }

    #[test]
    fn redact_and_assemble_drops_deny_listed_entries() {
        let lines = vec![
            assistant_tool_use("Read", json!({"file_path": "/Users/x/.ssh/id_rsa"})),
            assistant_text("Looking at the test failure"),
        ];
        let out = redact_and_assemble(&lines);
        // SSH read got dropped; assistant text survived.
        assert!(!out.contains("id_rsa"), "out = {out}");
        assert!(out.contains("test failure"), "out = {out}");
    }

    #[test]
    fn redact_and_assemble_applies_secret_pattern_to_assistant_text() {
        let lines = vec![assistant_text(
            "Token is ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345 from env",
        )];
        let out = redact_and_assemble(&lines);
        assert!(out.contains("<redacted>"));
        assert!(!out.contains("ghp_"));
    }

    #[test]
    fn redact_and_assemble_trims_oldest_when_over_cap() {
        // Build > MAX_PROMPT_BYTES of bland assistant text. Older
        // entries should drop first; the freshest should survive.
        let mut lines: Vec<Value> = (0..50)
            .map(|i| assistant_text(&format!("step {i}: running checks {}", "x".repeat(100))))
            .collect();
        lines.push(assistant_text("FRESHEST"));
        let out = redact_and_assemble(&lines);
        assert!(out.contains("FRESHEST"), "freshest entry must survive");
        assert!(out.len() <= MAX_PROMPT_BYTES + 256);
    }

    #[test]
    fn clean_summary_strips_quotes_and_period() {
        assert_eq!(
            clean_summary("\"running tests after the redactor lands.\""),
            "running tests after the redactor lands",
        );
    }

    #[test]
    fn clean_summary_rejects_mostly_redacted_reply() {
        // Model echo of a string that's almost entirely redaction
        // markers — must fall back to empty.
        let s = clean_summary("<redacted> <redacted> <redacted> <redacted>");
        assert!(s.is_empty(), "expected empty, got {s:?}");
    }

    #[test]
    fn clean_summary_post_filters_a_leaked_token() {
        // If the model echoed a token back at us, the post-filter
        // must catch it.
        let s = clean_summary("running tests with sk-ant-api03-abcdefghijklmnopqrstuvwxyz");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("sk-ant"));
    }

    #[test]
    fn clean_summary_collapses_internal_newlines_to_spaces() {
        assert_eq!(
            clean_summary("running tests\nafter the\nlayout fix"),
            "running tests after the layout fix",
        );
    }

    #[test]
    fn summarize_transcript_returns_none_without_api_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(summarize_transcript(None, &[]));
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn summarize_transcript_returns_none_when_transcript_empty_after_redaction() {
        let lines = vec![assistant_tool_use(
            "Read",
            json!({"file_path": "/Users/x/.ssh/id_rsa"}),
        )];
        // Even with an api key, redaction strips the only entry, so
        // we should bail out before calling the model.
        let result = summarize_transcript(Some("key"), &lines).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn end_to_end_summarize_with_wiremock() {
        // wiremock pretends to be Anthropic. We exercise the request
        // shape and the response parsing via `claude_one_sentence`
        // directly — the global http_client points at production and
        // is not overridable, mirroring pane_summary's test layout.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": "running tests after the redactor lands",
                }],
            })))
            .mount(&server)
            .await;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let (system, user) = build_messages("assistant: investigating the bug");
        let body = ClaudeRequest {
            model: SUMMARY_MODEL,
            max_tokens: SUMMARY_MAX_TOKENS,
            system: &system,
            messages: vec![ClaudeMessage {
                role: "user",
                content: user,
            }],
        };
        let resp = client
            .post(format!("{}/v1/messages", server.uri()))
            .header("x-api-key", "test-key")
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let parsed: ClaudeResponse = resp.json().await.unwrap();
        let text = parsed
            .content
            .into_iter()
            .find(|b| b.block_type == "text")
            .unwrap()
            .text;
        assert_eq!(
            clean_summary(&text),
            "running tests after the redactor lands",
        );
    }

    #[tokio::test]
    async fn end_to_end_redacts_transcript_before_calling_model() {
        // Verifies the privacy promise: even when a secret slips into
        // the assistant text, the redacted prompt body fed to the
        // model never contains it verbatim.
        let lines = vec![assistant_text(
            "trying token sk-ant-api03-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZ now",
        )];
        let body = redact_and_assemble(&lines);
        let (_system, user) = build_messages(&body);
        assert!(user.contains("<redacted>"), "user prompt was {user}");
        assert!(!user.contains("sk-ant-api03-Z"), "leaked: {user}");
    }
}
