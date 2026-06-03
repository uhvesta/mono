//! Specialised isolated Claude dispatch for the magic-wand affordance
//! (Phase 3 of `tools/boss/docs/designs/comments-in-markdown-viewer.md`).
//!
//! Makes a one-shot `messages.create` call against the Anthropic API with
//! **no tool surface** and returns the updated markdown. The model literally
//! cannot do anything except return text — no filesystem, no environment, no
//! conversation memory between invocations.
//!
//! Auth: a dedicated `BOSS_MAGIC_WAND_API_KEY` env var routes billing to a
//! separate spend bucket; falls back to `ANTHROPIC_API_KEY` when unset.
//! Design § "Billing and observability" and § "Constraint compliance".

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const MAGIC_WAND_API_KEY_ENV: &str = "BOSS_MAGIC_WAND_API_KEY";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Model used for magic-wand dispatch. Sonnet balances quality and latency
/// for the doc-editing task; the user-visible latency target is ~30 s.
const MAGIC_WAND_MODEL: &str = "claude-sonnet-4-6";
const MAGIC_WAND_MAX_TOKENS: u32 = 8192;
const MAGIC_WAND_TIMEOUT: Duration = Duration::from_secs(120);

// Hard-reject validation limits (design § "Validation before showing the preview").
const MIN_LENGTH_RATIO: f64 = 0.25;
const MAX_LENGTH_RATIO: f64 = 4.0;
const MAX_LINE_DIFF_FRACTION: f64 = 0.60;

/// Resolved outcome of a successful magic-wand dispatch.
pub struct MagicWandResult {
    pub result_md: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// `true` when the model returned a result but the anchor text is absent
    /// from the result AND wholesale changes occurred elsewhere. Surfaced as a
    /// warning in the preview sheet; never a hard reject.
    pub anchor_warning: bool,
}

/// Short error-kind values stored in `magic_wand_dispatches.error_kind`.
pub const ERROR_KIND_LENGTH_SANITY: &str = "length_sanity";
pub const ERROR_KIND_DIFF_SANITY: &str = "diff_sanity";
pub const ERROR_KIND_API: &str = "api_error";
pub const ERROR_KIND_EMPTY: &str = "empty_response";

// ── HTTP helpers ─────────────────────────────────────────────────────────────

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`; install a default
        // crypto provider before the first TLS handshake (same pattern as
        // `pane_summary.rs`).
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(MAGIC_WAND_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

pub fn resolve_api_key() -> Option<String> {
    std::env::var(MAGIC_WAND_API_KEY_ENV)
        .ok()
        .or_else(|| std::env::var(ANTHROPIC_API_KEY_ENV).ok())
}

// ── Prompt construction ───────────────────────────────────────────────────────

fn build_prompt(doc_text: &str, anchor_exact: &str, comment_body: &str) -> String {
    format!(
        "You are editing a markdown document. The user has highlighted a \
section and left a comment. Apply their intent to the document and \
return the entire updated markdown verbatim.\n\
\n\
Document:\n\
```markdown\n\
{doc_text}\n\
```\n\
\n\
Highlighted section:\n\
> {anchor_exact}\n\
\n\
Comment:\n\
> {comment_body}\n\
\n\
Respond with only the updated markdown. Do not include any \
explanation, header, or trailing prose."
    )
}

// ── Anthropic API types ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiContentBlock>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: i64,
    output_tokens: i64,
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Hard reject: length ratio outside [0.25×, 4×].
fn check_length_sanity(source: &str, result: &str) -> Result<()> {
    let src_len = source.len() as f64;
    if src_len == 0.0 {
        return Ok(());
    }
    let ratio = result.len() as f64 / src_len;
    if ratio < MIN_LENGTH_RATIO {
        anyhow::bail!(
            "result is too short (length ratio {:.2}; minimum {:.2}); \
             the model may have truncated the document",
            ratio,
            MIN_LENGTH_RATIO
        );
    }
    if ratio > MAX_LENGTH_RATIO {
        anyhow::bail!(
            "result is too long (length ratio {:.2}; maximum {:.2}); \
             the model may have repeated or appended to the document",
            ratio,
            MAX_LENGTH_RATIO
        );
    }
    Ok(())
}

/// Hard reject: >60% of source lines changed.
fn check_diff_sanity(source: &str, result: &str) -> Result<()> {
    let src_lines: Vec<&str> = source.lines().collect();
    if src_lines.is_empty() {
        return Ok(());
    }
    let fraction = changed_line_fraction(&src_lines, result);
    if fraction > MAX_LINE_DIFF_FRACTION {
        anyhow::bail!(
            "diff sanity: {:.0}% of lines changed (limit {:.0}%); \
             the model may have rewritten the entire document",
            fraction * 100.0,
            MAX_LINE_DIFF_FRACTION * 100.0
        );
    }
    Ok(())
}

/// Soft warning: anchor text is absent from the result AND widespread changes
/// occurred (>30% of lines changed). The user decides whether to apply.
fn anchor_warning(source: &str, result: &str, anchor_exact: &str) -> bool {
    if anchor_exact.is_empty() || result.contains(anchor_exact) {
        return false;
    }
    let src_lines: Vec<&str> = source.lines().collect();
    if src_lines.is_empty() {
        return false;
    }
    changed_line_fraction(&src_lines, result) > 0.30
}

/// Fraction of source lines that don't have a matching line in `result`.
/// Uses a multiset approach so duplicate lines are counted correctly.
fn changed_line_fraction(src_lines: &[&str], result: &str) -> f64 {
    use std::collections::HashMap;
    let mut res_counts: HashMap<&str, usize> = HashMap::new();
    for line in result.lines() {
        *res_counts.entry(line).or_default() += 1;
    }
    let mut unchanged = 0usize;
    for &line in src_lines {
        if let Some(count) = res_counts.get_mut(line)
            && *count > 0 {
                *count -= 1;
                unchanged += 1;
            }
    }
    (src_lines.len() - unchanged) as f64 / src_lines.len() as f64
}

// ── Public dispatch function ──────────────────────────────────────────────────

/// Make a one-shot `messages.create` call and return the validated result.
///
/// Errors are classified into `error_kind` values (`ERROR_KIND_*` constants)
/// so callers can persist the failure class alongside the error message.
///
/// Requires an Anthropic API key (resolved from `BOSS_MAGIC_WAND_API_KEY`
/// then `ANTHROPIC_API_KEY`). Returns an error if no key is found.
pub async fn dispatch(
    doc_text: &str,
    anchor_exact: &str,
    comment_body: &str,
) -> Result<MagicWandResult, (String, &'static str)> {
    let api_key = match resolve_api_key() {
        Some(k) => k,
        None => {
            return Err((
                "no Anthropic API key configured (set BOSS_MAGIC_WAND_API_KEY or \
                 ANTHROPIC_API_KEY)"
                    .to_owned(),
                ERROR_KIND_API,
            ));
        }
    };

    let prompt = build_prompt(doc_text, anchor_exact, comment_body);
    let body = ApiRequest {
        model: MAGIC_WAND_MODEL,
        max_tokens: MAGIC_WAND_MAX_TOKENS,
        messages: vec![ApiMessage {
            role: "user",
            content: prompt,
        }],
    };

    let resp = http_client()
        .post(ANTHROPIC_MESSAGES_URL)
        .header("x-api-key", &api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| (format!("HTTP send failed: {e}"), ERROR_KIND_API))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err((
            format!("Anthropic API returned {status}: {text}"),
            ERROR_KIND_API,
        ));
    }

    let parsed: ApiResponse = resp
        .json()
        .await
        .map_err(|e| (format!("failed to parse Anthropic response: {e}"), ERROR_KIND_API))?;

    let result_text = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .map(|b| b.text)
        .unwrap_or_default();

    let result_text = result_text.trim().to_owned();
    if result_text.is_empty() {
        return Err(("Anthropic returned an empty response".to_owned(), ERROR_KIND_EMPTY));
    }

    // Hard validation — length sanity first, then diff sanity.
    if let Err(e) = check_length_sanity(doc_text, &result_text) {
        return Err((e.to_string(), ERROR_KIND_LENGTH_SANITY));
    }
    if let Err(e) = check_diff_sanity(doc_text, &result_text) {
        return Err((e.to_string(), ERROR_KIND_DIFF_SANITY));
    }

    let warn = anchor_warning(doc_text, &result_text, anchor_exact);

    Ok(MagicWandResult {
        result_md: result_text,
        input_tokens: parsed.usage.input_tokens,
        output_tokens: parsed.usage.output_tokens,
        anchor_warning: warn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_sanity_within_bounds() {
        // identical content — ratio 1.0, always passes
        assert!(check_length_sanity("hello world", "hello world").is_ok());
    }

    #[test]
    fn length_sanity_too_short() {
        // 3 chars vs 100 chars → ratio 0.03, below 0.25
        let src = "a".repeat(100);
        assert!(check_length_sanity(&src, "abc").is_err());
    }

    #[test]
    fn length_sanity_too_long() {
        // 1000 chars vs 100 chars → ratio 10, above 4.0
        let src = "a".repeat(100);
        let result = "b".repeat(1000);
        assert!(check_length_sanity(&src, &result).is_err());
    }

    #[test]
    fn diff_sanity_no_changes() {
        let doc = "line1\nline2\nline3\n";
        assert!(check_diff_sanity(doc, doc).is_ok());
    }

    #[test]
    fn diff_sanity_wholesale_rewrite() {
        let src = (0..20).map(|i| format!("original line {i}")).collect::<Vec<_>>().join("\n");
        let result = (0..20).map(|i| format!("totally different {i}")).collect::<Vec<_>>().join("\n");
        assert!(check_diff_sanity(&src, &result).is_err());
    }

    #[test]
    fn diff_sanity_small_edit_passes() {
        let src = "intro\nbody line\nconclusion\n";
        let result = "intro\nbody line edited\nconclusion\n";
        assert!(check_diff_sanity(src, result).is_ok());
    }

    #[test]
    fn anchor_warning_triggered() {
        let src = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = (0..20).map(|i| format!("modified {i}")).collect::<Vec<_>>().join("\n");
        assert!(anchor_warning(&src, &result, "line 5"));
    }

    #[test]
    fn anchor_warning_not_triggered_when_anchor_survives() {
        let src = "keep this anchor\nother line\n";
        let result = "keep this anchor\nchanged line\n";
        assert!(!anchor_warning(src, result, "keep this anchor"));
    }

    #[test]
    fn build_prompt_contains_all_parts() {
        let p = build_prompt("## Doc\nContent", "Content", "Fix this please");
        assert!(p.contains("## Doc\nContent"));
        assert!(p.contains("> Content"));
        assert!(p.contains("> Fix this please"));
        // Verify no tool surface mention — the prompt is intentionally minimal.
        assert!(!p.contains("tool"));
    }
}
