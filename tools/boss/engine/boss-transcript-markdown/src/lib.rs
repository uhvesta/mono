//! JSONL → markdown transcript converter for Claude Code session logs.
//!
//! The public API is:
//! - [`parse_transcript`] — JSONL text → [`Vec<TranscriptEvent>`]
//! - [`events_to_segments`] — normalized events → [`Vec<TranscriptSegment>`]
//! - [`segments_to_markdown`] — flat document from segments (CLI / single-blob)
//! - [`render_text`] — plain-text rendering for the CLI transcript command

use serde_json::Value;

// ── Public types ──────────────────────────────────────────────────────────────

/// A normalized event parsed from one or more lines of a Claude Code JSONL
/// transcript file.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    pub seq: u64,
    pub kind: TranscriptEventKind,
    pub timestamp: Option<String>,
    pub model: Option<String>,
}

/// Discriminated kind for a transcript event.
#[derive(Debug, Clone)]
pub enum TranscriptEventKind {
    UserText(String),
    AssistantText(String),
    Thinking(String),
    ToolUse { name: String, input: Value },
    ToolResult { output: String, is_error: bool },
    System { subtype: Option<String>, body: String },
}

/// One rendered segment, suitable for lazy display in the UI.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct TranscriptSegment {
    pub seq: u64,
    pub role: SegmentRole,
    /// Short human-readable label (e.g. `"User"`, `"⚙ Bash"`, `"↳ result"`).
    pub label: String,
    pub timestamp: Option<String>,
    pub model: Option<String>,
    /// Rendered markdown body for this segment.
    pub markdown: String,
    #[builder(default = false)]
    pub collapsible: bool,
    #[builder(default = false)]
    pub default_collapsed: bool,
    pub truncated: Option<TruncationInfo>,
}

/// Role/origin of a transcript segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentRole {
    User,
    Assistant,
    Thinking,
    Tool,
    System,
}

/// Metadata set when a tool result was truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationInfo {
    pub shown_bytes: usize,
    pub total_bytes: usize,
}

/// Options controlling how events are rendered.
#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Maximum bytes from a single `tool_result` before the output is
    /// truncated and `truncated` is set on the segment.
    pub max_result_bytes: usize,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            max_result_bytes: 8 * 1024,
        }
    }
}

// ── JSONL parsing ─────────────────────────────────────────────────────────────

/// Parse raw JSONL transcript text into normalized events.
///
/// Each non-empty line is parsed as JSON. Malformed lines, unrecognised
/// types, and incomplete trailing lines are silently skipped — the caller
/// receives only well-formed events.
pub fn parse_transcript(jsonl_content: &str) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let mut seq: u64 = 0;
    for line in jsonl_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let new_events = parse_one_value(&value, &mut seq);
        events.extend(new_events);
    }
    events
}

fn parse_one_value(value: &Value, seq: &mut u64) -> Vec<TranscriptEvent> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let Some(type_str) = obj.get("type").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let timestamp = obj
        .get("timestamp")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    match type_str {
        "user" => parse_user_message(obj, timestamp, seq),
        "assistant" => parse_assistant_message(obj, timestamp, model, seq),
        "tool_result" => parse_tool_result(obj, timestamp, seq)
            .into_iter()
            .collect(),
        "system" => parse_system_event(obj, timestamp, seq)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_user_message(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let Some(message) = obj.get("message") else {
        return Vec::new();
    };
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    extract_text_blocks(content, "user", timestamp, None, seq)
}

fn parse_assistant_message(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    model: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let Some(message) = obj.get("message") else {
        return Vec::new();
    };
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    let model = model.or_else(|| {
        message
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
    });

    let mut events = Vec::new();
    if let Some(arr) = content.as_array() {
        for block in arr {
            let block_type = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text");
            match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let s = *seq;
                        *seq += 1;
                        events.push(TranscriptEvent {
                            seq: s,
                            kind: TranscriptEventKind::AssistantText(text.to_owned()),
                            timestamp: timestamp.clone(),
                            model: model.clone(),
                        });
                    }
                }
                "thinking" => {
                    if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                        let s = *seq;
                        *seq += 1;
                        events.push(TranscriptEvent {
                            seq: s,
                            kind: TranscriptEventKind::Thinking(thinking.to_owned()),
                            timestamp: timestamp.clone(),
                            model: model.clone(),
                        });
                    }
                }
                "tool_use" => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let s = *seq;
                    *seq += 1;
                    events.push(TranscriptEvent {
                        seq: s,
                        kind: TranscriptEventKind::ToolUse { name, input },
                        timestamp: timestamp.clone(),
                        model: model.clone(),
                    });
                }
                _ => {}
            }
        }
    } else if let Some(text) = content.as_str() {
        let s = *seq;
        *seq += 1;
        events.push(TranscriptEvent {
            seq: s,
            kind: TranscriptEventKind::AssistantText(text.to_owned()),
            timestamp,
            model,
        });
    }
    events
}

fn extract_text_blocks(
    content: &Value,
    role: &str,
    timestamp: Option<String>,
    model: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let make_kind: fn(String) -> TranscriptEventKind = if role == "user" {
        TranscriptEventKind::UserText
    } else {
        TranscriptEventKind::AssistantText
    };
    if let Some(arr) = content.as_array() {
        for block in arr {
            let bt = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text");
            if bt == "text" {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    let s = *seq;
                    *seq += 1;
                    events.push(TranscriptEvent {
                        seq: s,
                        kind: make_kind(text.to_owned()),
                        timestamp: timestamp.clone(),
                        model: model.clone(),
                    });
                }
            }
        }
    } else if let Some(text) = content.as_str() {
        let s = *seq;
        *seq += 1;
        events.push(TranscriptEvent {
            seq: s,
            kind: make_kind(text.to_owned()),
            timestamp,
            model,
        });
    }
    events
}

fn parse_tool_result(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Option<TranscriptEvent> {
    let output = if let Some(content) = obj.get("content") {
        if let Some(arr) = content.as_array() {
            arr.iter()
                .filter_map(|block| {
                    let bt = block
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text");
                    if bt == "text" {
                        block.get("text").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(text) = content.as_str() {
            text.to_owned()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Claude Code writes "isError" (camelCase); accept both spellings
    let is_error = obj
        .get("isError")
        .or_else(|| obj.get("is_error"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let s = *seq;
    *seq += 1;
    Some(TranscriptEvent {
        seq: s,
        kind: TranscriptEventKind::ToolResult { output, is_error },
        timestamp,
        model: None,
    })
}

fn parse_system_event(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Option<TranscriptEvent> {
    let subtype = obj
        .get("subtype")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    let body = match subtype.as_deref() {
        Some("pr-link") => {
            // Body is the raw PR URL
            obj.get("pr_url")
                .or_else(|| obj.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        }
        _ => {
            // Body = JSON of all fields except type/subtype/timestamp/sessionId
            let mut body_obj = serde_json::Map::new();
            for (k, v) in obj {
                if k != "type" && k != "subtype" && k != "timestamp" && k != "sessionId" {
                    body_obj.insert(k.clone(), v.clone());
                }
            }
            if body_obj.is_empty() {
                String::new()
            } else {
                serde_json::to_string_pretty(&Value::Object(body_obj)).unwrap_or_default()
            }
        }
    };

    let s = *seq;
    *seq += 1;
    Some(TranscriptEvent {
        seq: s,
        kind: TranscriptEventKind::System { subtype, body },
        timestamp,
        model: None,
    })
}

// ── events_to_segments ────────────────────────────────────────────────────────

/// Convert normalized transcript events into renderable segments.
pub fn events_to_segments(events: &[TranscriptEvent], opts: &RenderOpts) -> Vec<TranscriptSegment> {
    events
        .iter()
        .filter_map(|ev| event_to_segment(ev, opts))
        .collect()
}

fn event_to_segment(event: &TranscriptEvent, opts: &RenderOpts) -> Option<TranscriptSegment> {
    match &event.kind {
        TranscriptEventKind::UserText(text) => Some(
            TranscriptSegment::builder()
                .seq(event.seq)
                .role(SegmentRole::User)
                .label("User")
                .maybe_timestamp(event.timestamp.clone())
                .markdown(text.clone())
                .build(),
        ),

        TranscriptEventKind::AssistantText(text) => Some(
            TranscriptSegment::builder()
                .seq(event.seq)
                .role(SegmentRole::Assistant)
                .label("Assistant")
                .maybe_timestamp(event.timestamp.clone())
                .maybe_model(event.model.clone())
                .markdown(text.clone())
                .build(),
        ),

        TranscriptEventKind::Thinking(text) => {
            let markdown = blockquote(text);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::Thinking)
                    .label("💭 Thinking")
                    .maybe_timestamp(event.timestamp.clone())
                    .maybe_model(event.model.clone())
                    .markdown(markdown)
                    .collapsible(true)
                    .default_collapsed(true)
                    .build(),
            )
        }

        TranscriptEventKind::ToolUse { name, input } => {
            let markdown = render_tool_use(name, input);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::Tool)
                    .label(format!("⚙ {name}"))
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }

        TranscriptEventKind::ToolResult { output, is_error } => {
            render_tool_result_segment(event, output, *is_error, opts)
        }

        TranscriptEventKind::System { subtype, body } => {
            render_system_segment(event, subtype.as_deref(), body)
        }
    }
}

fn render_tool_use(name: &str, input: &Value) -> String {
    match name {
        "Bash" => {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("```sh\n{command}\n```")
        }
        "Edit" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let old = input.get("old_string").and_then(|v| v.as_str());
            let new = input.get("new_string").and_then(|v| v.as_str());
            match (old, new) {
                (Some(old_str), Some(new_str)) => format!(
                    "**Edit** `{path}`\n\n**Replace:**\n```\n{old_str}\n```\n\n**With:**\n```\n{new_str}\n```"
                ),
                _ => {
                    let json =
                        serde_json::to_string_pretty(input).unwrap_or_default();
                    format!("**Edit** `{path}`\n\n```json\n{json}\n```")
                }
            }
        }
        "Write" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("**Write** `{path}`\n\n```\n{content}\n```")
        }
        _ => {
            let json = serde_json::to_string_pretty(input).unwrap_or_default();
            format!("```json\n{json}\n```")
        }
    }
}

fn render_tool_result_segment(
    event: &TranscriptEvent,
    output: &str,
    is_error: bool,
    opts: &RenderOpts,
) -> Option<TranscriptSegment> {
    let total_bytes = output.len();
    let (shown_output, truncated) = if total_bytes > opts.max_result_bytes {
        let shown_len = safe_truncate_len(output, opts.max_result_bytes);
        (
            &output[..shown_len],
            Some(TruncationInfo {
                shown_bytes: shown_len,
                total_bytes,
            }),
        )
    } else {
        (output, None)
    };

    let error_marker = if is_error { "❌ **Error**\n\n" } else { "" };
    let markdown = format!("{error_marker}```\n{shown_output}\n```");
    let large = truncated.is_some() || total_bytes > 1024;

    Some(
        TranscriptSegment::builder()
            .seq(event.seq)
            .role(SegmentRole::Tool)
            .label("↳ result")
            .maybe_timestamp(event.timestamp.clone())
            .markdown(markdown)
            .collapsible(large)
            .maybe_truncated(truncated)
            .build(),
    )
}

fn render_system_segment(
    event: &TranscriptEvent,
    subtype: Option<&str>,
    body: &str,
) -> Option<TranscriptSegment> {
    match subtype {
        Some("init") => None,
        Some("pr-link") => {
            let markdown = if body.starts_with("http") {
                format!("[🔗 View PR]({body})")
            } else {
                body.to_owned()
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("🔗 PR")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some("stop_hook_summary") => {
            let markdown = if body.is_empty() {
                "> *(no summary)*".to_owned()
            } else {
                blockquote(body)
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("stop_hook_summary")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some("turn_duration") => {
            let markdown = if body.is_empty() {
                String::new()
            } else {
                blockquote(body)
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("turn_duration")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some(subtype_str) => {
            // Hook events, attachments, etc.
            let verbose = body.len() > 500;
            let markdown = render_body_as_markdown(body);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label(subtype_str.to_owned())
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .collapsible(verbose)
                    .build(),
            )
        }
        None => {
            let markdown = render_body_as_markdown(body);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("system")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
    }
}

// ── segments_to_markdown ──────────────────────────────────────────────────────

/// Flatten segments into a single markdown document (for the CLI
/// `--format=markdown` path and the single-blob `MarkdownDocRef` source).
pub fn segments_to_markdown(segs: &[TranscriptSegment]) -> String {
    let mut out = String::new();
    for seg in segs {
        out.push_str(&format!("## {}\n\n", segment_header(seg)));
        out.push_str(&seg.markdown);
        if !seg.markdown.ends_with('\n') {
            out.push('\n');
        }
        if let Some(t) = &seg.truncated {
            out.push_str(&format!(
                "\n*…showing {} of {} bytes*\n",
                t.shown_bytes, t.total_bytes
            ));
        }
        out.push('\n');
    }
    out
}

fn segment_header(seg: &TranscriptSegment) -> String {
    let mut parts = vec![seg.label.clone()];
    if let Some(ts) = &seg.timestamp {
        parts.push(ts.clone());
    }
    if let Some(model) = &seg.model {
        parts.push(format!("*{model}*"));
    }
    parts.join(" · ")
}

// ── render_text (plain-text CLI renderer) ─────────────────────────────────────

/// Render transcript events as plain text for the CLI `agents transcript`
/// command (format=text).
pub fn render_text(events: &[TranscriptEvent]) -> String {
    let opts = RenderOpts::default();
    let segs = events_to_segments(events, &opts);
    let mut out = String::new();
    for seg in &segs {
        let header = segment_header(seg);
        out.push_str(&format!("=== {header} ===\n"));
        out.push_str(&strip_markdown(&seg.markdown));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if let Some(t) = &seg.truncated {
            out.push_str(&format!(
                "[…showing {} of {} bytes]\n",
                t.shown_bytes, t.total_bytes
            ));
        }
        out.push('\n');
    }
    out
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn blockquote(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_body_as_markdown(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let trimmed = body.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        format!("```json\n{trimmed}\n```")
    } else {
        blockquote(trimmed)
    }
}

fn strip_markdown(md: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if trimmed.starts_with("> ") {
            out.push_str(&trimmed[2..]);
        } else if trimmed.starts_with('>') {
            out.push_str(&trimmed[1..]);
        } else if trimmed.starts_with("**") && trimmed.ends_with("**") {
            out.push_str(&trimmed[2..trimmed.len() - 2]);
        } else {
            out.push_str(trimmed);
        }
        out.push('\n');
    }
    out
}

/// Return the largest byte index ≤ `max_bytes` that is a valid UTF-8
/// char boundary in `s`. Always returns a value in `0..=s.len()`.
fn safe_truncate_len(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut len = max_bytes;
    while len > 0 && !s.is_char_boundary(len) {
        len -= 1;
    }
    len
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_transcript ──────────────────────────────────────────────────────

    #[test]
    fn parses_user_text_message() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello!"}]},"timestamp":"2024-01-01T00:00:00.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::UserText(t) => assert_eq!(t, "Hello!"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(events[0].timestamp.as_deref(), Some("2024-01-01T00:00:00.000Z"));
    }

    #[test]
    fn parses_assistant_text_message() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}]},"model":"claude-sonnet-4-6","timestamp":"2024-01-01T00:00:01.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::AssistantText(t) => assert_eq!(t, "Hi there!"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(events[0].model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn parses_thinking_block() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me reason about this."},{"type":"text","text":"Answer."}]},"model":"claude-sonnet-4-6"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, TranscriptEventKind::Thinking(_)));
        assert!(matches!(events[1].kind, TranscriptEventKind::AssistantText(_)));
    }

    #[test]
    fn parses_tool_use_bash() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls -la"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_use_edit() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/foo.rs","old_string":"let x","new_string":"let y"}}]}}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolUse { name, .. } => assert_eq!(name, "Edit"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result_ok() {
        let jsonl = r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"file.txt\ndir/"}],"isError":false,"timestamp":"2024-01-01T00:00:03.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolResult { output, is_error } => {
                assert!(output.contains("file.txt"));
                assert!(!is_error);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result_error() {
        let jsonl = r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"command not found"}],"isError":true}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolResult { is_error, .. } => assert!(is_error),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_init_event() {
        let jsonl = r#"{"type":"system","subtype":"init","cwd":"/workspace","timestamp":"2024-01-01T00:00:00.000Z","model":"claude-sonnet-4-6"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("init"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_pr_link() {
        let jsonl = r#"{"type":"system","subtype":"pr-link","pr_url":"https://github.com/foo/bar/pull/1","timestamp":"2024-01-01T00:00:10.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, body } => {
                assert_eq!(subtype.as_deref(), Some("pr-link"));
                assert_eq!(body, "https://github.com/foo/bar/pull/1");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_stop_hook_summary() {
        let jsonl = r#"{"type":"system","subtype":"stop_hook_summary","summary":"Task complete.","timestamp":"2024-01-01T00:01:00.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("stop_hook_summary"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_turn_duration() {
        let jsonl = r#"{"type":"system","subtype":"turn_duration","duration_ms":1234}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("turn_duration"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn skips_malformed_lines() {
        let jsonl = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{not valid json\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}";
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn skips_unknown_type() {
        let jsonl = r#"{"type":"unknown_type","data":"whatever"}"#;
        let events = parse_transcript(jsonl);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_empty_lines() {
        let jsonl = "\n\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn seq_increments_across_events() {
        let jsonl = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}}\n",
            "{\"type\":\"tool_result\",\"content\":[{\"type\":\"text\",\"text\":\"c\"}],\"isError\":false}"
        );
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
        assert_eq!(events[2].seq, 2);
    }

    // ── events_to_segments ────────────────────────────────────────────────────

    #[test]
    fn user_text_segment() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::UserText("Hello".to_owned()),
            timestamp: Some("2024-01-01T00:00:00.000Z".to_owned()),
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].role, SegmentRole::User);
        assert_eq!(segs[0].label, "User");
        assert_eq!(segs[0].markdown, "Hello");
        assert!(!segs[0].collapsible);
    }

    #[test]
    fn assistant_text_segment_carries_model() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::AssistantText("Hi".to_owned()),
            timestamp: None,
            model: Some("claude-sonnet-4-6".to_owned()),
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Assistant);
        assert_eq!(segs[0].model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn thinking_segment_is_collapsible_and_collapsed() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::Thinking("my thoughts".to_owned()),
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Thinking);
        assert!(segs[0].collapsible);
        assert!(segs[0].default_collapsed);
        assert!(segs[0].markdown.contains("> my thoughts"));
    }

    #[test]
    fn bash_tool_use_renders_sh_fence() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Bash".to_owned(),
                input: serde_json::json!({"command": "echo hello"}),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Tool);
        assert!(segs[0].markdown.contains("```sh\necho hello\n```"));
    }

    #[test]
    fn edit_tool_use_renders_path_and_diff() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Edit".to_owned(),
                input: serde_json::json!({
                    "file_path": "/src/main.rs",
                    "old_string": "let x = 1;",
                    "new_string": "let x = 2;"
                }),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        let md = &segs[0].markdown;
        assert!(md.contains("`/src/main.rs`"), "got: {md}");
        assert!(md.contains("let x = 1;"), "got: {md}");
        assert!(md.contains("let x = 2;"), "got: {md}");
    }

    #[test]
    fn write_tool_use_renders_path_and_content() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Write".to_owned(),
                input: serde_json::json!({
                    "file_path": "/out.txt",
                    "content": "hello world"
                }),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        let md = &segs[0].markdown;
        assert!(md.contains("`/out.txt`"), "got: {md}");
        assert!(md.contains("hello world"), "got: {md}");
    }

    #[test]
    fn unknown_tool_use_renders_json_fence() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Read".to_owned(),
                input: serde_json::json!({"file_path": "/foo.rs"}),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs[0].markdown.contains("```json"));
    }

    #[test]
    fn tool_result_ok_not_collapsed_when_small() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: "ok".to_owned(),
                is_error: false,
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Tool);
        assert_eq!(segs[0].label, "↳ result");
        assert!(!segs[0].collapsible);
        assert!(segs[0].truncated.is_none());
    }

    #[test]
    fn tool_result_error_adds_marker() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: "not found".to_owned(),
                is_error: true,
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs[0].markdown.contains("❌"));
    }

    #[test]
    fn tool_result_truncated_when_over_limit() {
        let big = "x".repeat(20_000);
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: big.clone(),
                is_error: false,
            },
            timestamp: None,
            model: None,
        }];
        let opts = RenderOpts { max_result_bytes: 1024 };
        let segs = events_to_segments(&events, &opts);
        assert!(segs[0].collapsible);
        let t = segs[0].truncated.as_ref().expect("truncated should be set");
        assert_eq!(t.shown_bytes, 1024);
        assert_eq!(t.total_bytes, 20_000);
    }

    #[test]
    fn system_init_is_skipped() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("init".to_owned()),
                body: String::new(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs.is_empty(), "init events should be filtered");
    }

    #[test]
    fn system_pr_link_renders_markdown_link() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("pr-link".to_owned()),
                body: "https://github.com/foo/bar/pull/42".to_owned(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].label, "🔗 PR");
        assert!(segs[0].markdown.contains("[🔗 View PR]"));
        assert!(segs[0].markdown.contains("https://github.com/foo/bar/pull/42"));
    }

    #[test]
    fn system_stop_hook_summary_renders_blockquote() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("stop_hook_summary".to_owned()),
                body: "All done.".to_owned(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].label, "stop_hook_summary");
        assert!(segs[0].markdown.starts_with("> "));
    }

    // ── segments_to_markdown ──────────────────────────────────────────────────

    #[test]
    fn segments_to_markdown_produces_h2_headers() {
        let segs = vec![TranscriptSegment::builder()
            .seq(0)
            .role(SegmentRole::User)
            .label("User")
            .markdown("Hello")
            .build()];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("## User\n\nHello"), "got: {md}");
    }

    #[test]
    fn segments_to_markdown_includes_timestamp_in_header() {
        let segs = vec![TranscriptSegment::builder()
            .seq(0)
            .role(SegmentRole::Assistant)
            .label("Assistant")
            .timestamp("2024-01-01T00:00:01Z")
            .model("claude-sonnet-4-6")
            .markdown("Reply")
            .build()];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("2024-01-01T00:00:01Z"));
        assert!(md.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn segments_to_markdown_adds_truncation_note() {
        let segs = vec![TranscriptSegment::builder()
            .seq(0)
            .role(SegmentRole::Tool)
            .label("↳ result")
            .markdown("```\nshort\n```")
            .collapsible(true)
            .maybe_truncated(Some(TruncationInfo {
                shown_bytes: 100,
                total_bytes: 5000,
            }))
            .build()];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("showing 100 of 5000 bytes"), "got: {md}");
    }

    // ── render_text ───────────────────────────────────────────────────────────

    #[test]
    fn render_text_produces_plain_text() {
        let events = parse_transcript(concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}"
        ));
        let text = render_text(&events);
        assert!(text.contains("=== User ==="), "got: {text}");
        assert!(text.contains("hi"), "got: {text}");
        assert!(text.contains("=== Assistant ==="), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
    }

    // ── safe_truncate_len ─────────────────────────────────────────────────────

    #[test]
    fn safe_truncate_len_respects_char_boundaries() {
        // "é" is 2 bytes (0xC3 0xA9). Truncating at byte 1 would be invalid.
        let s = "aé";
        assert_eq!(s.len(), 3); // 'a'=1, 'é'=2
        assert_eq!(safe_truncate_len(s, 2), 1); // can't split 'é', so stop at 'a'
        assert_eq!(safe_truncate_len(s, 3), 3);
        assert_eq!(safe_truncate_len(s, 10), 3);
    }
}
