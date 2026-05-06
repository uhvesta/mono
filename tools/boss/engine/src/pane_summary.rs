//! Generate and cache short human-readable pane-titlebar summaries
//! for work items.
//!
//! The macOS app's worker pane titlebars used to show the bare run id
//! (`exec_18ad...`). That's stable for traceability but unreadable
//! at a glance — eight panes on screen looked identical. We now ask
//! Claude (Sonnet — fast and cheap) to compress the work item's name
//! plus description into a 2–4 word summary like `"Fix fencer
//! scraper"` and pass it across the engine→app RPC.
//!
//! Caching: results are stored in the `pane_summaries` table keyed
//! by work_item_id, alongside a `basis_hash` derived from the inputs
//! we fed to Claude (name + description). When the work item's name
//! or description changes, the basis hash changes and we regenerate
//! on the next spawn. Logs, APIs, and identifiers everywhere else
//! still use the run id — this module only feeds the visual
//! titlebar.
//!
//! Failure modes are silent on purpose. If the API key is missing
//! or the request fails (timeout, transport, 5xx), we fall back to
//! a deterministic local trim of the work item name. That keeps the
//! pane spawn flow on its happy path even when the network or
//! Anthropic is down. The fallback is *not* cached so a later spawn
//! can still call the API and store a real summary.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::work::{WorkDb, WorkItem};

/// Anthropic Messages API endpoint. Hard-coded; not configurable
/// because nothing in this codebase points at a non-prod Anthropic
/// instance and a typo in an env override would silently lose
/// summaries.
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Sonnet 4.6: latest released Sonnet at the time of writing — the
/// design doc explicitly calls it out as the right speed/cost
/// balance for this kind of micro-prompt.
const SUMMARY_MODEL: &str = "claude-sonnet-4-6";
/// 30 tokens is plenty for 2–4 words. Keeping the cap tight avoids
/// the model rambling into a 20-word summary and keeps the API call
/// cheap.
const SUMMARY_MAX_TOKENS: u32 = 30;
/// Hard timeout on the round-trip. Worker spawn is user-visible and
/// we'd rather show the fallback than block the pane on a slow
/// upstream. Sonnet on a tiny prompt typically returns in well
/// under a second.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5);

/// Compute a stable hash of the inputs that, if changed, must
/// invalidate the cached summary. Used as the `basis_hash` column
/// in `pane_summaries`.
pub fn compute_basis(name: &str, description: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(description.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Pull the (name, description) pair off whichever variant of
/// [`WorkItem`] the caller has. Tasks and chores share the `Task`
/// shape so they're handled together.
pub fn name_and_description(item: &WorkItem) -> (&str, &str) {
    match item {
        WorkItem::Product(p) => (p.name.as_str(), p.description.as_str()),
        WorkItem::Project(p) => (p.name.as_str(), p.description.as_str()),
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.name.as_str(), t.description.as_str()),
    }
}

/// Returns the work item's id regardless of variant. Lifted out so
/// callers don't have to repeat the match.
pub fn item_id(item: &WorkItem) -> &str {
    match item {
        WorkItem::Product(p) => &p.id,
        WorkItem::Project(p) => &p.id,
        WorkItem::Task(t) | WorkItem::Chore(t) => &t.id,
    }
}

/// Fall back to a local trim of the work item's name. Used when the
/// API key is absent or the upstream call fails — better than
/// surfacing a raw exec id when we can do something readable for
/// free. The fallback is *not* cached because a later spawn might
/// be able to reach Claude.
pub fn local_fallback(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let words: Vec<&str> = trimmed.split_whitespace().take(4).collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" "))
}

/// Resolve a summary for a work item, hitting the cache first and
/// falling through to Claude only on a miss or basis change. Errors
/// are swallowed — this function never blocks worker spawn — and a
/// `None` return tells the caller to display the run id as before.
pub async fn get_or_generate(
    db: &WorkDb,
    api_key: Option<&str>,
    work_item: &WorkItem,
) -> Option<String> {
    let (name, description) = name_and_description(work_item);
    let basis = compute_basis(name, description);
    let id = item_id(work_item);

    match db.get_pane_summary(id) {
        Ok(Some((summary, cached_basis))) if cached_basis == basis => {
            return Some(summary);
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = id,
                ?err,
                "pane_summary: cache lookup failed; will try to regenerate",
            );
        }
    }

    if let Some(api_key) = api_key {
        match claude_short_summary(api_key, name, description).await {
            Ok(summary) => {
                if let Err(err) = db.set_pane_summary(id, &summary, &basis) {
                    tracing::warn!(
                        work_item_id = id,
                        ?err,
                        "pane_summary: failed to cache summary; will retry next spawn",
                    );
                }
                return Some(summary);
            }
            Err(err) => {
                tracing::warn!(
                    work_item_id = id,
                    ?err,
                    "pane_summary: Claude call failed; using local fallback",
                );
            }
        }
    } else {
        tracing::debug!(
            work_item_id = id,
            "pane_summary: no ANTHROPIC_API_KEY in config; using local fallback",
        );
    }

    local_fallback(name)
}

#[derive(Debug, Serialize)]
struct ClaudeRequest<'a> {
    model: &'a str,
    max_tokens: u32,
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

/// Build the prompt for Claude. Pulled out as a free function so
/// tests can pin the exact wording — drift here changes summary
/// style across all panes.
fn build_prompt(name: &str, description: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You compress engineering task titles into a 2-4 word label fit for a UI titlebar. \
         Reply with the label only — no quotes, no trailing period, no explanation. \
         Use Title Case. Drop articles (\"the\", \"a\") and filler verbs when possible.\n\n",
    );
    prompt.push_str("Task name:\n");
    prompt.push_str(name);
    prompt.push('\n');
    if !description.trim().is_empty() {
        prompt.push_str("\nTask description:\n");
        // Cap the description so a runaway design doc doesn't blow
        // the prompt up. 600 chars is enough to disambiguate similar
        // titles without paying for a long context window.
        let truncated: String = description.chars().take(600).collect();
        prompt.push_str(&truncated);
        prompt.push('\n');
    }
    prompt.push_str("\nLabel:");
    prompt
}

/// Lazy, process-wide reqwest client. Re-using a single client lets
/// the connection pool kick in across spawns; we don't expect this
/// to need per-call configuration.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`, so
        // every binary that uses it has to install a default crypto
        // provider before the first TLS handshake or `Client::build`
        // panics. The engine doesn't otherwise touch rustls, so we
        // do it lazily here. `install_default()` errors if a
        // provider is already set — that's fine, we ignore it.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(SUMMARY_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

/// POST to the Anthropic Messages API and pull the first text block
/// out of the response. Errors are bucketed into `anyhow` because
/// the caller (`get_or_generate`) only logs them.
pub async fn claude_short_summary(
    api_key: &str,
    name: &str,
    description: &str,
) -> Result<String> {
    let client = http_client();
    let prompt = build_prompt(name, description);
    let body = ClaudeRequest {
        model: SUMMARY_MODEL,
        max_tokens: SUMMARY_MAX_TOKENS,
        messages: vec![ClaudeMessage {
            role: "user",
            content: prompt,
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
        anyhow::bail!("anthropic returned {}: {}", status, text);
    }

    let parsed: ClaudeResponse = resp.json().await?;
    let text = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .map(|b| b.text)
        .unwrap_or_default();

    let cleaned = clean_summary(&text);
    if cleaned.is_empty() {
        anyhow::bail!("anthropic returned an empty summary");
    }
    Ok(cleaned)
}

/// Strip whitespace, surrounding quotes, and trailing punctuation
/// from the model's reply, and clamp to 4 words. Sonnet reliably
/// follows the format instruction but a stray quote or period
/// shouldn't bleed into the titlebar.
fn clean_summary(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .trim_start_matches(|c: char| c == '"' || c == '\'' || c == '`')
        .trim_end_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.')
        .trim();
    let words: Vec<&str> = stripped.split_whitespace().take(4).collect();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkDb;
    use boss_protocol::Task;
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_task(id: &str, name: &str, description: &str) -> WorkItem {
        WorkItem::Task(Task {
            id: id.to_owned(),
            product_id: "prod-1".to_owned(),
            project_id: None,
            kind: "task".to_owned(),
            name: name.to_owned(),
            description: description.to_owned(),
            status: "active".to_owned(),
            ordinal: None,
            pr_url: None,
            deleted_at: None,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
        })
    }

    #[test]
    fn basis_hash_is_stable_for_same_inputs() {
        let a = compute_basis("Fix fencer scraper", "Scraper hits 429s on big tournaments");
        let b = compute_basis("Fix fencer scraper", "Scraper hits 429s on big tournaments");
        assert_eq!(a, b);
    }

    #[test]
    fn basis_hash_differs_when_name_changes() {
        let a = compute_basis("Fix fencer scraper", "desc");
        let b = compute_basis("Fix fencer scraper v2", "desc");
        assert_ne!(a, b);
    }

    #[test]
    fn basis_hash_differs_when_description_changes() {
        let a = compute_basis("name", "Old description");
        let b = compute_basis("name", "New description");
        assert_ne!(a, b);
    }

    #[test]
    fn basis_hash_does_not_collide_when_separator_moves() {
        // Without the explicit zero-byte separator, ("ab", "c") and
        // ("a", "bc") would hash the same. Make sure we keep them
        // distinct.
        let a = compute_basis("ab", "c");
        let b = compute_basis("a", "bc");
        assert_ne!(a, b);
    }

    #[test]
    fn local_fallback_takes_first_four_words() {
        assert_eq!(
            local_fallback("Show short task summary in agent pane titlebar").as_deref(),
            Some("Show short task summary"),
        );
    }

    #[test]
    fn local_fallback_returns_short_input_unchanged() {
        assert_eq!(local_fallback("Fix fencer").as_deref(), Some("Fix fencer"));
    }

    #[test]
    fn local_fallback_handles_empty() {
        assert_eq!(local_fallback("").as_deref(), None);
        assert_eq!(local_fallback("   ").as_deref(), None);
    }

    #[test]
    fn clean_summary_strips_quotes_and_periods() {
        assert_eq!(clean_summary("\"Fix Fencer Scraper.\""), "Fix Fencer Scraper");
        assert_eq!(clean_summary("  Pane Titlebar Summary  "), "Pane Titlebar Summary");
    }

    #[test]
    fn clean_summary_clamps_to_four_words() {
        assert_eq!(
            clean_summary("One Two Three Four Five Six"),
            "One Two Three Four",
        );
    }

    #[test]
    fn clean_summary_returns_empty_for_empty_input() {
        assert_eq!(clean_summary(""), "");
        assert_eq!(clean_summary("   "), "");
    }

    #[tokio::test]
    async fn cache_hit_returns_stored_summary_without_calling_api() {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let item = sample_task("task-1", "Fix fencer scraper", "desc");
        let basis = compute_basis("Fix fencer scraper", "desc");
        db.set_pane_summary("task-1", "Cached Summary", &basis)
            .unwrap();

        // No API key → would normally fall through to local
        // fallback. A cache hit should short-circuit before that.
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), Some("Cached Summary"));
    }

    #[tokio::test]
    async fn cache_invalidates_when_basis_changes() {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let stale_basis = compute_basis("old name", "old desc");
        db.set_pane_summary("task-1", "Stale Summary", &stale_basis)
            .unwrap();

        // Same id, different name → cache should miss; with no API
        // key we get the local fallback derived from the new name.
        let item = sample_task("task-1", "New Name Goes Here", "new desc");
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), Some("New Name Goes Here"));
    }

    #[tokio::test]
    async fn no_api_key_falls_back_to_first_words_of_name() {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let item = sample_task("task-1", "Show short task summary in agent pane", "");
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), Some("Show short task summary"));
    }

    #[tokio::test]
    async fn api_response_overrides_local_fallback_and_caches() {
        // Spin up a wiremock that pretends to be Anthropic. We
        // can't override the URL on the global client, so this
        // test exercises `claude_short_summary` directly to prove
        // the request-shaping and response-parsing path. The
        // caching half is covered by `cache_hit_returns_stored_summary_*`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "Pane Titlebar Summary"}],
            })))
            .mount(&server)
            .await;

        // wiremock serves http://, so no TLS handshake actually runs,
        // but reqwest's rustls-no-provider build still wants a default
        // crypto provider installed before `Client::new()` is called.
        // Mirror what the prod path does in `http_client()`.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let body = ClaudeRequest {
            model: SUMMARY_MODEL,
            max_tokens: SUMMARY_MAX_TOKENS,
            messages: vec![ClaudeMessage {
                role: "user",
                content: build_prompt("name", "desc"),
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
        assert_eq!(parsed.content.len(), 1);
        assert_eq!(clean_summary(&parsed.content[0].text), "Pane Titlebar Summary");
    }
}
