//! Layered redactor used by the live-status summarizer.
//!
//! Transcripts contain raw tool input/output: file contents, bash
//! output, environment-variable dumps, fetched HTTP bodies. A naive
//! summarizer prompt would happily quote a file path, an env var, or
//! an API token directly into the status string, where it ends up on
//! the kanban board, which gets screenshotted, which gets shared.
//!
//! This module is the engine-side defence layer. It is intentionally
//! a pure function over `&str` (and a corresponding helper for
//! pre-stripping large `tool_input` / `tool_response` blobs out of
//! transcript JSONL values). No state, no I/O — fully unit-testable
//! against synthetic-secret samples.
//!
//! Posture: prefer false positive to false negative. Catching a stack
//! trace and replacing a long hex digest inside it with `<redacted>`
//! is fine; failing to catch a real secret because the regex was too
//! narrow is not. The redactor is one of three layers from the
//! design (Q8 in `tools/boss/docs/designs/worker-live-status.md`) —
//! prompt guardrails and a post-output filter sit either side of the
//! actual model call.
//!
//! Layers implemented here:
//!
//! - [`redact_text`] — apply the secret-pattern regexes to any string.
//!   Used for both the redacted transcript window fed to the model
//!   *and* the model's reply before it's written to `live_status`.
//! - [`truncate_large_values`] — collapse any `tool_input` or
//!   `tool_response` JSON value longer than [`MAX_VALUE_BYTES`] to a
//!   single `<truncated>` placeholder. Removes the "we just sent the
//!   whole file contents to the summarizer" attack surface before any
//!   string scanning happens.
//! - [`should_drop_entry`] — return `true` for transcript JSONL
//!   entries whose tool name or argument path matches a deny-list
//!   (anything reading `~/.config`, `/Users/*/secrets`, env-var dumps
//!   matching common token names). Caller drops the entry entirely
//!   rather than redacting it.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// Placeholder used wherever a redactor pattern matched. Deliberately
/// short and visually distinct so the summarizer prompt can warn the
/// model that "any `<redacted>` marker is the censored value, not the
/// intended noun".
pub const REDACTED: &str = "<redacted>";

/// Placeholder used by [`truncate_large_values`] for blobs that
/// exceeded the per-value byte ceiling.
pub const TRUNCATED: &str = "<truncated>";

/// Per-value byte ceiling for `tool_input` / `tool_response` JSON
/// payloads inside a transcript line. Anything longer (a file body, a
/// big bash dump) is replaced by [`TRUNCATED`] before pattern
/// matching even runs. 2 KiB matches the figure called out in the
/// design doc and is comfortably below the per-call input cap fed to
/// the model.
pub const MAX_VALUE_BYTES: usize = 2 * 1024;

/// Worst-case fraction of a redacted string that may be the
/// [`REDACTED`] / [`TRUNCATED`] placeholders before the output filter
/// gives up and drops the value entirely. The summarizer module
/// consults [`is_mostly_redacted`] when deciding whether the model
/// reply is salvageable.
pub const MAX_REDACTION_RATIO: f32 = 0.9;

/// Single-pass redactor: run every pattern in [`secret_patterns`]
/// against `input` and return a string with every match replaced by
/// [`REDACTED`].
///
/// Patterns are deliberately broad. False positives (e.g., a stack
/// trace containing a 40-char hex digest becomes `<redacted>`) are
/// fine; false negatives are not.
pub fn redact_text(input: &str) -> String {
    let mut out = input.to_owned();
    for re in secret_patterns() {
        out = re.replace_all(&out, REDACTED).into_owned();
    }
    out
}

/// True iff `s` is empty after trimming, or [`REDACTED`] /
/// [`TRUNCATED`] markers fill more than [`MAX_REDACTION_RATIO`] of
/// its bytes. Used by the post-output filter to recognise an
/// unrecoverable response — if the model echoed back a string that
/// was 90% censored markers, the summarizer drops it and the UI
/// keeps the prior value rather than rendering noise.
pub fn is_mostly_redacted(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return true;
    }
    let total = trimmed.len() as f32;
    let redacted_chars =
        (count_occurrences(trimmed, REDACTED) * REDACTED.len()) as f32 +
        (count_occurrences(trimmed, TRUNCATED) * TRUNCATED.len()) as f32;
    redacted_chars / total > MAX_REDACTION_RATIO
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut idx = 0usize;
    while let Some(pos) = haystack[idx..].find(needle) {
        count += 1;
        idx += pos + needle.len();
    }
    count
}

/// Walk a JSONL transcript line and replace any `tool_input` or
/// `tool_response` value whose serialised length exceeds
/// [`MAX_VALUE_BYTES`] with a `<truncated>` string.
///
/// The function mutates a copy and returns it, so the caller can keep
/// the original line around for debugging if needed. We only need to
/// inspect a fixed set of top-level keys: the claude transcript
/// format puts the tool payload either at the top level (legacy
/// shape) or inside the assistant message's content blocks
/// (`type == "tool_use"` / `"tool_result"`). Both shapes are handled.
pub fn truncate_large_values(mut value: Value) -> Value {
    truncate_in_place(&mut value);
    value
}

fn truncate_in_place(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj.iter_mut() {
                if matches!(k.as_str(), "tool_input" | "tool_response" | "input" | "content")
                    && value_byte_len(v) > MAX_VALUE_BYTES
                {
                    *v = Value::String(TRUNCATED.to_owned());
                } else {
                    truncate_in_place(v);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                truncate_in_place(v);
            }
        }
        _ => {}
    }
}

fn value_byte_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        // Cheap upper bound: serialise back to JSON only when we
        // could not have decided from the variant alone.
        _ => serde_json::to_string(v).map(|s| s.len()).unwrap_or(0),
    }
}

/// Decide whether to drop a transcript JSONL entry whole rather than
/// summarising any of its content. Used for tool calls that read
/// known-sensitive locations (env-var dumps, `~/.config`,
/// `/Users/*/secrets`). Coarser than [`redact_text`] but cheaper —
/// the safest thing the redactor can do is to never let the bytes
/// hit the summarizer in the first place.
///
/// The check looks at three things, in order:
/// 1. The top-level `tool_name` (legacy claude transcript shape).
/// 2. Any content block with `type == "tool_use"` whose `name` is on
///    the deny-list.
/// 3. The `command` / `path` / `file_path` argument inside a tool
///    payload, when the tool itself is benign (e.g. `Read`) but the
///    target is sensitive.
pub fn should_drop_entry(value: &Value) -> bool {
    if let Some(name) = value.get("tool_name").and_then(|v| v.as_str()) {
        if tool_name_is_sensitive(name) {
            return true;
        }
        if let Some(input) = value.get("tool_input") {
            if input_targets_sensitive_path(name, input) {
                return true;
            }
        }
    }
    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        for block in content {
            let Some(obj) = block.as_object() else { continue };
            let block_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
            if block_type == "tool_use" {
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
                if tool_name_is_sensitive(name) {
                    return true;
                }
                if let Some(input) = obj.get("input") {
                    if input_targets_sensitive_path(name, input) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Tool names that should never have their payload exposed to the
/// summarizer regardless of arguments. Anything that reads env vars
/// or runs an env-dumping shell snippet would be a Bash call, so we
/// don't blanket-deny Bash — instead the env-var deny check happens
/// inside [`input_targets_sensitive_path`] on the Bash command body.
fn tool_name_is_sensitive(_name: &str) -> bool {
    // Reserved: leave as a hook so future deny-listed tool kinds can
    // be added without changing call sites. Today every sensitive
    // case is path/argument-driven and handled inline.
    false
}

/// Inspect a tool input payload's path/command argument for known
/// sensitive locations or env-var dump shell snippets.
fn input_targets_sensitive_path(tool_name: &str, input: &Value) -> bool {
    let path_candidates = match tool_name {
        "Read" | "Write" | "Edit" | "MultiEdit" | "Glob" | "Grep" | "NotebookRead"
        | "NotebookEdit" => vec![
            input.get("file_path").and_then(Value::as_str),
            input.get("path").and_then(Value::as_str),
            input.get("notebook_path").and_then(Value::as_str),
        ],
        "Bash" => {
            // For Bash, also walk the command body for env-var dumps
            // or accesses to sensitive paths. Cheap substring scan
            // — a real shell parser would be overkill.
            if let Some(cmd) = input.get("command").and_then(Value::as_str) {
                if bash_command_is_sensitive(cmd) {
                    return true;
                }
                vec![Some(cmd)]
            } else {
                vec![]
            }
        }
        _ => vec![],
    };
    for candidate in path_candidates.into_iter().flatten() {
        if path_is_sensitive(candidate) {
            return true;
        }
    }
    false
}

/// True iff `path` (or a Bash command containing one) hits a known-
/// sensitive directory. Matched as substring rather than a glob to
/// keep the check cheap and intentionally over-eager.
fn path_is_sensitive(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    SENSITIVE_PATH_FRAGMENTS
        .iter()
        .any(|frag| lower.contains(frag))
}

const SENSITIVE_PATH_FRAGMENTS: &[&str] = &[
    "/.ssh/",
    "/.aws/",
    "/.config/",
    "/.gnupg/",
    "/secrets/",
    "/secret/",
    "/.netrc",
    "/.npmrc",
    "/.pypirc",
    "/private/etc/master.passwd",
];

/// True iff a Bash `command` argument is doing something we don't
/// want surfaced — printing the environment, cat'ing well-known
/// secret files, etc. Matched as case-insensitive substring; this
/// produces some false positives (`env` is also a binary name in
/// many languages) but the redactor goal is conservative.
fn bash_command_is_sensitive(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    // Common token env var names that, if printed, dump credentials.
    const TOKEN_ENV_NAMES: &[&str] = &[
        "anthropic_api_key",
        "openai_api_key",
        "github_token",
        "gh_token",
        "aws_secret_access_key",
        "aws_session_token",
        "boss_api_token",
        "boss_admin_token",
        "slack_token",
    ];
    if TOKEN_ENV_NAMES.iter().any(|n| lower.contains(n)) {
        return true;
    }
    // Common "dump everything" shapes.
    const DUMP_FRAGMENTS: &[&str] = &[
        "printenv",
        "env | grep",
        "env|grep",
        "set | grep",
        "echo $",
    ];
    if DUMP_FRAGMENTS.iter().any(|f| lower.contains(f)) {
        return true;
    }
    // Cat'ing well-known credential files.
    if lower.contains("cat ")
        && SENSITIVE_PATH_FRAGMENTS
            .iter()
            .any(|frag| lower.contains(frag))
    {
        return true;
    }
    false
}

/// Lazily-compiled secret patterns. Layered conservatively per Q8 of
/// the design doc — broad patterns first (specific prefixes) then a
/// long-hex / long-base64 catch-all that intentionally over-redacts.
fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw_patterns: &[&str] = &[
            // Anthropic API keys (`sk-ant-...` and the older `sk-...`).
            r"sk-[A-Za-z0-9_\-]{20,}",
            // GitHub personal access tokens (modern + legacy prefixes).
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            // AWS access key ids (well-known prefix + body shape).
            r"AKIA[0-9A-Z]{16}",
            r"ASIA[0-9A-Z]{16}",
            // Bearer tokens — anything that quacks like one.
            r"(?i)Bearer\s+[A-Za-z0-9._\-]{16,}",
            // Generic key/value pairs that say "password=foo" or
            // "token: bar" — match the value, not the label.
            r"(?i)\b(password|passwd|pwd|api[_\-]?key|secret|token)\s*[:=]\s*[^\s,;]{6,}",
            // AWS-shaped key/value pairs without the prefixes above.
            r"(?i)aws_[a-z_]*key[a-z_]*\s*[:=]\s*\S+",
            // Slack bot/user tokens.
            r"xox[abprs]-[A-Za-z0-9\-]{10,}",
            // Generic 40+ character hex run (anything that looks like
            // a SHA-1+ digest or a hex-encoded credential).
            r"\b[0-9a-fA-F]{40,}\b",
            // Generic 32+ character base64-like blob. Limited to
            // boundaries so common identifiers and run ids escape.
            r"\b[A-Za-z0-9+/]{32,}={0,2}\b",
            // Workspace paths — leak a task name from the dir name.
            r"/Users/[^/\s]+/Documents/dev/workspaces/[^\s]+",
            // SSH-style paths and dotfile homes.
            r"/Users/[^/\s]+/\.(ssh|aws|gnupg|config|netrc|npmrc)[^\s]*",
        ];
        raw_patterns
            .iter()
            .map(|p| Regex::new(p).expect("secret pattern must compile"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_anthropic_api_key() {
        let input = "Found key sk-ant-api03-AAAAbbbbCCCCddddEEEEffff1234 in the file.";
        let out = redact_text(input);
        assert!(out.contains(REDACTED), "out = {out}");
        assert!(!out.contains("sk-ant-api03"), "out = {out}");
    }

    #[test]
    fn redacts_github_token() {
        let input = "header Authorization: token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        let out = redact_text(input);
        assert!(out.contains(REDACTED), "out = {out}");
        assert!(!out.contains("ghp_"), "out = {out}");
    }

    #[test]
    fn redacts_aws_access_key_id() {
        let out = redact_text("akid AKIAIOSFODNN7EXAMPLE done");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_bearer_authorization_header() {
        let out = redact_text("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc.def-ghi");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("eyJ"));
    }

    #[test]
    fn redacts_inline_password_assignment() {
        let out = redact_text("connecting with password=hunter2-bigger! to db");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn redacts_long_hex_digest() {
        let out =
            redact_text("digest e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn redacts_workspace_paths() {
        let out = redact_text(
            "writing to /Users/brian/Documents/dev/workspaces/mono-agent-003/foo/bar",
        );
        assert!(out.contains(REDACTED));
        assert!(!out.contains("mono-agent-003"));
    }

    #[test]
    fn redacts_ssh_dir_paths() {
        let out = redact_text("reading /Users/alice/.ssh/id_rsa");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("id_rsa"));
    }

    #[test]
    fn redacts_slack_tokens() {
        let out = redact_text("slack xoxb-1234-abcdefghij notify");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("xoxb-"));
    }

    #[test]
    fn leaves_innocuous_text_alone() {
        let input = "running cargo test for boss-engine completion";
        let out = redact_text(input);
        assert_eq!(out, input);
    }

    #[test]
    fn is_mostly_redacted_flags_pure_redaction() {
        assert!(is_mostly_redacted("<redacted><redacted>"));
        assert!(is_mostly_redacted("    "));
        assert!(!is_mostly_redacted("running tests after the redacted fix"));
    }

    #[test]
    fn truncate_large_string_value_in_tool_response() {
        let big = "x".repeat(MAX_VALUE_BYTES + 10);
        let line = json!({
            "type": "user",
            "tool_name": "Read",
            "tool_response": big,
        });
        let out = truncate_large_values(line);
        assert_eq!(out["tool_response"], TRUNCATED);
    }

    #[test]
    fn truncate_keeps_small_values_intact() {
        let line = json!({
            "type": "user",
            "tool_name": "Read",
            "tool_response": "hello",
        });
        let out = truncate_large_values(line);
        assert_eq!(out["tool_response"], "hello");
    }

    #[test]
    fn truncate_walks_into_content_blocks_and_truncates_large_input() {
        let big = "y".repeat(MAX_VALUE_BYTES + 10);
        let line = json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": big},
                }],
            },
        });
        let out = truncate_large_values(line);
        // The container "content" key got pattern-matched and the
        // whole array exceeds the cap — verify the top-level walk
        // collapsed it to a sentinel.
        assert_eq!(out["message"]["content"], TRUNCATED);
    }

    #[test]
    fn drop_entry_when_tool_reads_secrets_dir() {
        let line = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "/Users/alice/secrets/api.txt"},
        });
        assert!(should_drop_entry(&line));
    }

    #[test]
    fn drop_entry_when_tool_reads_config_dir() {
        let line = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "/Users/alice/.config/claude/auth.json"},
        });
        assert!(should_drop_entry(&line));
    }

    #[test]
    fn drop_entry_when_bash_dumps_env() {
        let line = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "printenv | grep API"},
        });
        assert!(should_drop_entry(&line));
    }

    #[test]
    fn drop_entry_when_bash_references_known_token_env_var() {
        let line = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "curl -H \"X-API-Key: $ANTHROPIC_API_KEY\" ..."},
        });
        assert!(should_drop_entry(&line));
    }

    #[test]
    fn drop_entry_handles_assistant_tool_use_blocks() {
        let line = json!({
            "type": "assistant",
            "content": [{
                "type": "tool_use",
                "name": "Read",
                "input": {"file_path": "/Users/x/.ssh/id_rsa"},
            }],
        });
        assert!(should_drop_entry(&line));
    }

    #[test]
    fn keep_entry_when_tool_path_is_benign() {
        let line = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "tools/boss/engine/src/app.rs"},
        });
        assert!(!should_drop_entry(&line));
    }

    /// Smoke corpus: redactor must catch ≥95% of these synthetic
    /// secret samples, per the design's Layer-1 acceptance bar.
    #[test]
    fn redacts_synthetic_secret_corpus() {
        let samples: &[&str] = &[
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234567890",
            "ghp_1234567890abcdefghij1234567890abcdefgh",
            "gho_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "ghs_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "AKIAIOSFODNN7EXAMPLE",
            "Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature",
            "password = topsecret-blah",
            "api_key: tok_abcdef123456",
            "secret=plaintextsecretvalue",
            "xoxb-1234567890-ABCDEFGHIJKL",
            "1234567890abcdef1234567890abcdef12345678",
            "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG",
            "/Users/brian/Documents/dev/workspaces/mono-agent-005/foo",
            "/Users/brian/.ssh/id_rsa",
            "https://example.com/path?token=abcdef0123456789abcdef0123456789",
            "AAAAB3NzaC1yc2EAAAADAQABAAABAQ==",
            "/Users/brian/.aws/credentials",
            "/Users/brian/.config/claude/api.json",
            "Authorization: token ghu_HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH",
            "ASIAYDFAKEFAKEFAKEEF",
        ];
        let total = samples.len();
        let mut caught = 0usize;
        for s in samples {
            let out = redact_text(s);
            if out.contains(REDACTED) {
                caught += 1;
            }
        }
        let ratio = caught as f32 / total as f32;
        assert!(
            ratio >= 0.95,
            "redactor caught {caught}/{total} = {ratio:.2}, expected >= 0.95"
        );
    }
}
