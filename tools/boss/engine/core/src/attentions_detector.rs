//! Structured-first creation pipeline for Attentions
//! (design: `tools/boss/docs/designs/attentions.md`, "Creation pipeline").
//!
//! Two structured producers feed the engine-owned store
//! ([`WorkDb::reconcile_attentions`]):
//!
//! - **Questions** — a `kind=design` worker emits a sibling questions
//!   manifest at `<slug>.attentions.json` next to its design doc. When the
//!   design PR is detected (or merged), [`reconcile_design_doc_questions`]
//!   fetches the manifest from the PR branch, parses it, and upserts a
//!   `question|{project_id}|doc:{path}` group plus its members.
//! - **Followups** — an implementation worker emits a `FOLLOWUPS:` sentinel
//!   followed by a fenced JSON array near the end of its final response.
//!   At completion, [`reconcile_task_followups`] reads the transcript tail,
//!   parses the block, and upserts a `followup|{task_id}` group.
//!
//! Both paths are content-idempotent: re-detecting the same PR or
//! re-emitting the same block never appends duplicate members (the dedup
//! lives in [`WorkDb::reconcile_attentions`]). Extraction backstops for
//! manifest-less docs / sentinel-less transcripts are a separate, flag-gated
//! concern — [`extract_doc_questions_backstop`] and
//! [`extract_followups_backstop`] — disabled by default.

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use boss_protocol::{Attention, AttentionGroup, CreateAttentionInput};
use boss_transcript_markdown::{TranscriptEventKind, parse_transcript};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::design_detector;
use crate::work::WorkDb;

// ── Backstop: Anthropic API constants ────────────────────────────────────────

const BACKSTOP_API_KEY_ENV: &str = "BOSS_BACKSTOP_API_KEY";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const BACKSTOP_MODEL: &str = "claude-haiku-4-5-20251001";
const BACKSTOP_MAX_TOKENS: u32 = 2048;
const BACKSTOP_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum number of transcript characters fed to the supervisor pass.
const BACKSTOP_TRANSCRIPT_TAIL_CHARS: usize = 8_000;
/// Maximum number of questions the backstop will emit from one doc.
const BACKSTOP_MAX_QUESTIONS: usize = 20;

fn backstop_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(BACKSTOP_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

fn resolve_backstop_api_key() -> Option<String> {
    std::env::var(BACKSTOP_API_KEY_ENV)
        .ok()
        .or_else(|| std::env::var(ANTHROPIC_API_KEY_ENV).ok())
}

#[derive(Serialize)]
struct BackstopApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<BackstopApiMessage<'a>>,
}

#[derive(Serialize)]
struct BackstopApiMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct BackstopApiResponse {
    content: Vec<BackstopApiContentBlock>,
}

#[derive(Deserialize)]
struct BackstopApiContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
}

/// One entry of a `<slug>.attentions.json` questions manifest.
#[derive(Debug, Clone, Deserialize)]
struct QuestionManifestEntry {
    /// `yes_no` | `multiple_choice` | `prompt`.
    question_type: String,
    /// The question shown to the human.
    prompt: String,
    /// Choices for `multiple_choice`; ignored for other types.
    #[serde(default)]
    choices: Option<Vec<String>>,
    /// Heading slug the question pertains to (drives inline placement).
    #[serde(default)]
    anchor: Option<String>,
}

/// One entry of a `FOLLOWUPS:` block.
#[derive(Debug, Clone, Deserialize)]
struct FollowupEntry {
    /// Pre-fills the task name (required).
    proposed_name: String,
    #[serde(default)]
    proposed_description: Option<String>,
    /// `trivial` … `max`; passed through untouched.
    #[serde(default)]
    proposed_effort: Option<String>,
    /// `task` | `chore` | `project`; dropped if not one of those.
    #[serde(default)]
    proposed_work_kind: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
}

// ── Questions: design-doc manifest ──────────────────────────────────────────

/// Fired from `completion::finalize_pr_transition` for a `kind=design` task.
/// Scans the PR for its single design doc, fetches the sibling
/// `<slug>.attentions.json` from the PR branch, and upserts the question
/// group + members. Returns the group and the members newly inserted on this
/// call (so the caller can push `AttentionCreated`), or `None` when there is
/// no manifest / no new questions. All failures are logged and swallowed —
/// they must never mask the surrounding PR transition.
pub async fn reconcile_design_doc_questions(
    work_db: &WorkDb,
    task_id: &str,
    project_id: &str,
    pr_url: &str,
    merged: bool,
) -> Option<(AttentionGroup, Vec<Attention>)> {
    let scan = design_detector::scan_pr(task_id, pr_url).await?;
    let doc_path = scan.doc_path?;
    let manifest_path = sibling_manifest_path(&doc_path)?;
    let (owner, repo) = match parse_owner_repo_from_pr_url(pr_url) {
        Some(or) => or,
        None => {
            tracing::warn!(
                task_id,
                pr_url,
                "attentions detector: cannot parse owner/repo from PR URL; skipping question manifest"
            );
            return None;
        }
    };

    // Prefer the head branch while the PR is open (the doc lives there);
    // prefer the base branch once merged (the head may be deleted). Try both
    // so a fast create-and-merge still resolves.
    let mut branches: Vec<String> = Vec::new();
    let (primary, secondary) = if merged {
        (scan.base_ref_name.clone(), scan.head_ref_name.clone())
    } else {
        (scan.head_ref_name.clone(), scan.base_ref_name.clone())
    };
    for b in [primary, secondary].into_iter().flatten() {
        if !b.is_empty() && !branches.contains(&b) {
            branches.push(b);
        }
    }

    let mut found: Option<(String, String)> = None;
    for branch in &branches {
        match fetch_pr_file(&owner, &repo, &manifest_path, branch).await {
            Ok(Some(content)) => {
                found = Some((content, branch.clone()));
                break;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    task_id,
                    manifest_path,
                    branch,
                    ?err,
                    "attentions detector: failed to fetch question manifest"
                );
            }
        }
    }
    let (raw, branch) = found?;

    let entries = match parse_question_manifest(&raw) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                task_id,
                manifest_path,
                ?err,
                "attentions detector: question manifest is not valid JSON; skipping"
            );
            return None;
        }
    };

    let repo_remote_url = format!("https://github.com/{owner}/{repo}");
    let inputs: Vec<CreateAttentionInput> = entries
        .iter()
        .filter_map(|entry| {
            build_question_input(entry, project_id, task_id, &doc_path, &repo_remote_url, &branch)
        })
        .collect();
    if inputs.is_empty() {
        return None;
    }

    match work_db.reconcile_attentions(inputs) {
        Ok(Some((group, created))) => {
            if !created.is_empty() {
                tracing::info!(
                    task_id,
                    project_id,
                    group_id = %group.id,
                    new_members = created.len(),
                    "attentions detector: upserted design-doc question group"
                );
            }
            Some((group, created))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                task_id,
                project_id,
                ?err,
                "attentions detector: failed to reconcile design-doc questions"
            );
            None
        }
    }
}

/// Build a `question` [`CreateAttentionInput`] from a manifest entry, or
/// `None` when the entry is malformed (unknown `question_type`, empty prompt,
/// or a `multiple_choice` with no choices). Filtering here keeps the store's
/// batch reconcile from aborting on a single bad entry.
fn build_question_input(
    entry: &QuestionManifestEntry,
    project_id: &str,
    task_id: &str,
    doc_path: &str,
    repo_remote_url: &str,
    branch: &str,
) -> Option<CreateAttentionInput> {
    let question_type = entry.question_type.trim();
    if !matches!(question_type, "yes_no" | "multiple_choice" | "prompt") {
        tracing::warn!(
            question_type,
            "attentions detector: skipping manifest entry with unknown question_type"
        );
        return None;
    }
    if entry.prompt.trim().is_empty() {
        return None;
    }
    let choice_options = if question_type == "multiple_choice" {
        let choices = entry.choices.as_ref().filter(|c| !c.is_empty())?;
        Some(serde_json::to_string(choices).ok()?)
    } else {
        None
    };
    let anchor = entry
        .anchor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    Some(
        CreateAttentionInput::builder()
            .kind("question")
            .association_project_id(project_id)
            .source_kind("design_doc")
            .source_task_id(task_id)
            .source_doc_path(doc_path)
            .source_doc_repo_remote_url(repo_remote_url)
            .source_doc_branch(branch)
            .question_type(question_type)
            .prompt_text(entry.prompt.trim())
            .maybe_choice_options(choice_options)
            .maybe_source_anchor(anchor)
            .confidence_source("structured")
            .build(),
    )
}

/// Parse a `<slug>.attentions.json` manifest body (a JSON array of entries).
fn parse_question_manifest(raw: &str) -> serde_json::Result<Vec<QuestionManifestEntry>> {
    serde_json::from_str(raw)
}

/// `tools/boss/docs/designs/foo.md` → `tools/boss/docs/designs/foo.attentions.json`.
/// `None` when `doc_path` does not end in `.md` / `.markdown`.
fn sibling_manifest_path(doc_path: &str) -> Option<String> {
    let stem = doc_path
        .strip_suffix(".markdown")
        .or_else(|| doc_path.strip_suffix(".md"))?;
    Some(format!("{stem}.attentions.json"))
}

/// `https://github.com/OWNER/REPO/pull/123` → `("OWNER", "REPO")`.
fn parse_owner_repo_from_pr_url(pr_url: &str) -> Option<(String, String)> {
    let (owner, repo) = git_utils::repo_slug::parse_github_owner_repo(pr_url).ok()?;
    Some((owner.to_owned(), repo.to_owned()))
}

/// Fetch one file's raw contents from `owner/repo@branch` via the GitHub
/// Contents API. Returns `Ok(None)` when the file does not exist (a 404 —
/// the common "no manifest" case), `Err` only on a real tool/transport
/// failure. `--method GET` is required so `-f ref=` lands in the query
/// string (gh otherwise switches to POST once a field is added), which also
/// makes gh URL-encode slashed branch names like `boss/exec_*` correctly.
async fn fetch_pr_file(
    owner: &str,
    repo: &str,
    path: &str,
    branch: &str,
) -> anyhow::Result<Option<String>> {
    let endpoint = format!("repos/{owner}/{repo}/contents/{path}");
    let output = Command::new("gh")
        .args([
            "api",
            &endpoint,
            "--method",
            "GET",
            "-f",
            &format!("ref={branch}"),
            "-H",
            "Accept: application/vnd.github.raw",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Not Found") || stderr.contains("404") {
        return Ok(None);
    }
    anyhow::bail!("`gh api {endpoint}` failed: {}", stderr.trim());
}

// ── Followups: transcript-tail sentinel ─────────────────────────────────────

/// Fired from `completion::finalize_pr_transition` for any completing work
/// item. Reads the run transcript, extracts the worker's assistant text, and
/// upserts a `followup|{work_item_id}` group from any `FOLLOWUPS:` block.
/// Returns the group + newly-inserted members, or `None` when there is no
/// transcript / no block / no new followups. Failures are logged and
/// swallowed.
pub async fn reconcile_task_followups(
    work_db: &WorkDb,
    work_item_id: &str,
    execution_id: &str,
    transcript_path: Option<&str>,
) -> Option<(AttentionGroup, Vec<Attention>)> {
    let path = transcript_path?;
    let jsonl = match tokio::fs::read_to_string(path).await {
        Ok(jsonl) => jsonl,
        Err(err) => {
            tracing::debug!(
                execution_id,
                path,
                ?err,
                "attentions detector: could not read transcript for followups"
            );
            return None;
        }
    };

    let assistant_text = extract_assistant_text(&jsonl);
    let entries = parse_followups_block(&assistant_text);
    if entries.is_empty() {
        return None;
    }

    let inputs: Vec<CreateAttentionInput> = entries
        .iter()
        .filter_map(|entry| build_followup_input(entry, work_item_id, execution_id))
        .collect();
    if inputs.is_empty() {
        return None;
    }

    match work_db.reconcile_attentions(inputs) {
        Ok(Some((group, created))) => {
            if !created.is_empty() {
                tracing::info!(
                    work_item_id,
                    execution_id,
                    group_id = %group.id,
                    new_members = created.len(),
                    "attentions detector: upserted task followup group"
                );
            }
            Some((group, created))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                execution_id,
                ?err,
                "attentions detector: failed to reconcile task followups"
            );
            None
        }
    }
}

/// Build a `followup` [`CreateAttentionInput`], or `None` when the entry has
/// no name. An unknown `proposed_work_kind` is dropped (left to the store
/// default) rather than rejected.
fn build_followup_input(
    entry: &FollowupEntry,
    work_item_id: &str,
    execution_id: &str,
) -> Option<CreateAttentionInput> {
    let name = entry.proposed_name.trim();
    if name.is_empty() {
        return None;
    }
    let work_kind = entry
        .proposed_work_kind
        .as_deref()
        .map(str::trim)
        .filter(|s| matches!(*s, "task" | "chore" | "project"))
        .map(str::to_owned);
    let effort = entry
        .proposed_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let description = entry
        .proposed_description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let rationale = entry
        .rationale
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    Some(
        CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id(work_item_id)
            .source_kind("task_transcript")
            .source_task_id(work_item_id)
            .source_run_id(execution_id)
            .proposed_name(name)
            .maybe_proposed_description(description)
            .maybe_proposed_effort(effort)
            .maybe_proposed_work_kind(work_kind)
            .maybe_rationale(rationale)
            .confidence_source("structured")
            .build(),
    )
}

/// Concatenate the worker's assistant-authored text from a JSONL transcript.
/// We scan only assistant text (never the user prompt or tool output) so the
/// `FOLLOWUPS:` instructions in the worker's *prompt* can never be mistaken
/// for an emitted block.
fn extract_assistant_text(jsonl: &str) -> String {
    parse_transcript(jsonl)
        .into_iter()
        .filter_map(|event| match event.kind {
            TranscriptEventKind::AssistantText(text) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the `FOLLOWUPS:` block out of assistant text: locate the last
/// `FOLLOWUPS:` sentinel and the first balanced JSON array after it (fenced
/// or not). Returns an empty `Vec` when no parseable block is present.
fn parse_followups_block(text: &str) -> Vec<FollowupEntry> {
    let Some(idx) = text.rfind("FOLLOWUPS:") else {
        return Vec::new();
    };
    let tail = &text[idx..];
    let Some(array) = extract_balanced_array(tail) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<FollowupEntry>>(&array) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(?err, "attentions detector: FOLLOWUPS block is not a valid JSON array");
            Vec::new()
        }
    }
}

/// Return the first balanced `[...]` JSON array in `s` (depth-counting,
/// string- and escape-aware so brackets inside string literals are ignored).
/// Handles fenced blocks transparently — the fence markers sit outside the
/// array.
fn extract_balanced_array(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'[')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..start + offset + 1].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

// ── Backstop: Questions extraction ───────────────────────────────────────────

/// Graceful-degradation backstop for docs that ship no `<slug>.attentions.json`
/// manifest. Scans the PR for a design doc, fetches its content, and extracts
/// list items from the "Risks / open questions" section as `prompt`-type
/// attentions with `confidence_source = extracted`. Only called when the
/// primary [`reconcile_design_doc_questions`] returned `None`.
///
/// All failures are logged and swallowed — the backstop must never mask the
/// surrounding completion path.
pub async fn extract_doc_questions_backstop(
    work_db: &WorkDb,
    task_id: &str,
    project_id: &str,
    pr_url: &str,
    merged: bool,
) -> Option<(AttentionGroup, Vec<Attention>)> {
    let scan = design_detector::scan_pr(task_id, pr_url).await?;
    let doc_path = scan.doc_path?;

    let (owner, repo) = match parse_owner_repo_from_pr_url(pr_url) {
        Some(or) => or,
        None => {
            tracing::warn!(
                task_id,
                pr_url,
                "attentions backstop (questions): cannot parse owner/repo from PR URL"
            );
            return None;
        }
    };

    let mut branches: Vec<String> = Vec::new();
    let (primary, secondary) = if merged {
        (scan.base_ref_name.clone(), scan.head_ref_name.clone())
    } else {
        (scan.head_ref_name.clone(), scan.base_ref_name.clone())
    };
    for b in [primary, secondary].into_iter().flatten() {
        if !b.is_empty() && !branches.contains(&b) {
            branches.push(b);
        }
    }

    let mut found: Option<(String, String)> = None;
    for branch in &branches {
        match fetch_pr_file(&owner, &repo, &doc_path, branch).await {
            Ok(Some(content)) => {
                found = Some((content, branch.clone()));
                break;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    task_id,
                    doc_path,
                    branch,
                    ?err,
                    "attentions backstop (questions): failed to fetch design doc"
                );
            }
        }
    }
    let (doc_content, branch) = found?;

    let items = extract_risks_section_items(&doc_content);
    if items.is_empty() {
        return None;
    }

    let repo_remote_url = format!("https://github.com/{owner}/{repo}");
    let inputs: Vec<CreateAttentionInput> = items
        .into_iter()
        .take(BACKSTOP_MAX_QUESTIONS)
        .map(|item| {
            CreateAttentionInput::builder()
                .kind("question")
                .association_project_id(project_id)
                .source_kind("design_doc")
                .source_task_id(task_id)
                .source_doc_path(doc_path.as_str())
                .source_doc_repo_remote_url(repo_remote_url.as_str())
                .source_doc_branch(branch.as_str())
                .source_anchor("risks-open-questions")
                .question_type("prompt")
                .prompt_text(item.as_str())
                .confidence_source("extracted")
                .build()
        })
        .collect();

    match work_db.reconcile_attentions(inputs) {
        Ok(Some((group, created))) => {
            if !created.is_empty() {
                tracing::info!(
                    task_id,
                    project_id,
                    group_id = %group.id,
                    new_members = created.len(),
                    "attentions backstop (questions): upserted extracted question group"
                );
            }
            Some((group, created))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                task_id,
                project_id,
                ?err,
                "attentions backstop (questions): failed to reconcile extracted questions"
            );
            None
        }
    }
}

/// Extract list items from the first "Risks / open questions" section of a
/// markdown doc. Matches headings like "Risks / open questions", "Risks/Open
/// Questions", "Open Questions", "Risks", etc. (case-insensitive). Each
/// numbered or bulleted list item becomes one string; heading bold is stripped.
/// Returns an empty `Vec` when no matching section is found.
fn extract_risks_section_items(doc: &str) -> Vec<String> {
    let mut in_section = false;
    let mut items: Vec<String> = Vec::new();

    for line in doc.lines() {
        let trimmed = line.trim();

        // Detect ATX headings (`#`, `##`, etc.).
        if trimmed.starts_with('#') {
            let heading_text = trimmed.trim_start_matches('#').trim().to_lowercase();
            if is_risks_heading(&heading_text) {
                in_section = true;
                continue;
            } else if in_section {
                // Next heading terminates the section.
                break;
            }
            continue;
        }

        if !in_section {
            continue;
        }

        // Numbered list items: "1. text" or "1) text".
        if let Some(rest) = strip_numbered_list_prefix(trimmed) {
            let text = strip_markdown_bold(rest).trim().to_owned();
            if !text.is_empty() {
                items.push(text);
            }
            continue;
        }

        // Bulleted list items: "- text" or "* text" or "+ text".
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let text = strip_markdown_bold(rest).trim().to_owned();
            if !text.is_empty() {
                items.push(text);
            }
        }
    }

    items
}

/// `true` when a heading text (already lowercased) looks like a Risks or
/// Open Questions section.
fn is_risks_heading(heading_lower: &str) -> bool {
    let normalised = heading_lower.replace(['/', '-', '_'], " ");
    let normalised = normalised.trim();
    matches!(
        normalised,
        "risks" | "open questions" | "open question" | "risks open questions"
            | "risks   open questions"
            | "risks and open questions"
    ) || normalised.contains("open question")
        || (normalised.contains("risk") && normalised.contains("question"))
}

/// Strip `N.` or `N)` list prefix from `s`, returning the rest or `None`.
fn strip_numbered_list_prefix(s: &str) -> Option<&str> {
    let mut bytes_consumed = 0usize;
    let bytes = s.as_bytes();
    // Consume leading ASCII digits.
    while bytes_consumed < bytes.len() && bytes[bytes_consumed].is_ascii_digit() {
        bytes_consumed += 1;
    }
    if bytes_consumed == 0 {
        return None;
    }
    match bytes.get(bytes_consumed) {
        Some(b'.') | Some(b')') => bytes_consumed += 1,
        _ => return None,
    }
    let rest = &s[bytes_consumed..];
    Some(rest.trim_start())
}

/// Remove markdown bold markers (`**…**` and `__…__`) from text, collapsing
/// whitespace only minimally so content is preserved.
fn strip_markdown_bold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let marker = if (c == '*' && chars.peek() == Some(&'*'))
            || (c == '_' && chars.peek() == Some(&'_'))
        {
            chars.next(); // consume second char
            Some(c)
        } else {
            None
        };
        if marker.is_none() {
            out.push(c);
        }
    }
    out
}

// ── Backstop: Followups supervisor extraction ─────────────────────────────────

/// Graceful-degradation backstop for workers that complete without emitting a
/// structured `FOLLOWUPS:` block. Reads the transcript tail and asks a
/// lightweight supervisor LLM to extract candidate followups, flagged
/// `confidence_source = extracted`. Only called when the primary
/// [`reconcile_task_followups`] returned `None`.
///
/// Requires `BOSS_BACKSTOP_API_KEY` or `ANTHROPIC_API_KEY` to be set; logs a
/// warning and returns `None` when the key is absent. All other failures are
/// also logged and swallowed.
pub async fn extract_followups_backstop(
    work_db: &WorkDb,
    work_item_id: &str,
    execution_id: &str,
    transcript_path: Option<&str>,
) -> Option<(AttentionGroup, Vec<Attention>)> {
    let path = transcript_path?;
    let jsonl = match tokio::fs::read_to_string(path).await {
        Ok(jsonl) => jsonl,
        Err(err) => {
            tracing::debug!(
                execution_id,
                path,
                ?err,
                "attentions backstop (followups): could not read transcript"
            );
            return None;
        }
    };

    let assistant_text = extract_assistant_text(&jsonl);
    if assistant_text.is_empty() {
        return None;
    }

    // Feed only the tail so the LLM call stays cheap.
    let tail = if assistant_text.len() > BACKSTOP_TRANSCRIPT_TAIL_CHARS {
        &assistant_text[assistant_text.len() - BACKSTOP_TRANSCRIPT_TAIL_CHARS..]
    } else {
        &assistant_text
    };

    let api_key = match resolve_backstop_api_key() {
        Some(k) => k,
        None => {
            tracing::warn!(
                execution_id,
                "attentions backstop (followups): no API key configured \
                 (set BOSS_BACKSTOP_API_KEY or ANTHROPIC_API_KEY); skipping"
            );
            return None;
        }
    };

    let prompt = build_followups_supervisor_prompt(tail);
    let body = BackstopApiRequest {
        model: BACKSTOP_MODEL,
        max_tokens: BACKSTOP_MAX_TOKENS,
        messages: vec![BackstopApiMessage {
            role: "user",
            content: prompt,
        }],
    };

    let resp = match backstop_http_client()
        .post(ANTHROPIC_MESSAGES_URL)
        .header("x-api-key", &api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "attentions backstop (followups): HTTP send failed"
            );
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(
            execution_id,
            %status,
            text,
            "attentions backstop (followups): Anthropic API error"
        );
        return None;
    }

    let parsed: BackstopApiResponse = match resp.json().await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "attentions backstop (followups): failed to parse Anthropic response"
            );
            return None;
        }
    };

    let response_text = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .map(|b| b.text)
        .unwrap_or_default();

    let entries = parse_followups_block(&response_text);
    if entries.is_empty() {
        return None;
    }

    let inputs: Vec<CreateAttentionInput> = entries
        .iter()
        .filter_map(|entry| {
            let mut input = build_followup_input(entry, work_item_id, execution_id)?;
            input.confidence_source = Some("extracted".to_owned());
            Some(input)
        })
        .collect();
    if inputs.is_empty() {
        return None;
    }

    match work_db.reconcile_attentions(inputs) {
        Ok(Some((group, created))) => {
            if !created.is_empty() {
                tracing::info!(
                    work_item_id,
                    execution_id,
                    group_id = %group.id,
                    new_members = created.len(),
                    "attentions backstop (followups): upserted extracted followup group"
                );
            }
            Some((group, created))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                execution_id,
                ?err,
                "attentions backstop (followups): failed to reconcile extracted followups"
            );
            None
        }
    }
}

fn build_followups_supervisor_prompt(transcript_tail: &str) -> String {
    format!(
        "You are a supervisor reviewing the final portion of a software-engineering agent's \
transcript. Extract any concrete follow-on work items the agent noticed but did not complete. \
Return ONLY a JSON array — no explanation, no markdown fences. Each element must have: \
\"proposed_name\" (short task title, required), \"proposed_description\" (one paragraph scope, \
required), optionally \"proposed_effort\" (one of: trivial, small, medium, large, max), \
optionally \"proposed_work_kind\" (one of: task, chore, project), optionally \"rationale\" \
(why worth doing). If there are no concrete follow-on items, return an empty array [].\n\
\n\
Transcript tail:\n\
---\n\
{transcript_tail}\n\
---"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_manifest_path_swaps_extension() {
        assert_eq!(
            sibling_manifest_path("tools/boss/docs/designs/attentions.md").as_deref(),
            Some("tools/boss/docs/designs/attentions.attentions.json")
        );
        assert_eq!(
            sibling_manifest_path("docs/x.markdown").as_deref(),
            Some("docs/x.attentions.json")
        );
        assert_eq!(sibling_manifest_path("docs/x.txt"), None);
    }

    #[test]
    fn parses_owner_repo_from_pr_url() {
        assert_eq!(
            parse_owner_repo_from_pr_url("https://github.com/spinyfin/mono/pull/991"),
            Some(("spinyfin".to_owned(), "mono".to_owned()))
        );
        assert_eq!(parse_owner_repo_from_pr_url("not a url"), None);
    }

    #[test]
    fn parses_question_manifest() {
        let raw = r#"[
            {"question_type": "yes_no", "prompt": "Gate behind a flag?", "anchor": "rollout"},
            {"question_type": "multiple_choice", "prompt": "How many tables?",
             "choices": ["one", "two"], "anchor": "schema"}
        ]"#;
        let entries = parse_question_manifest(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].question_type, "yes_no");
        assert_eq!(entries[1].choices.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn build_question_input_rejects_bad_entries() {
        let bad_type = QuestionManifestEntry {
            question_type: "essay".to_owned(),
            prompt: "?".to_owned(),
            choices: None,
            anchor: None,
        };
        assert!(build_question_input(&bad_type, "P", "T", "d.md", "r", "b").is_none());

        let mc_no_choices = QuestionManifestEntry {
            question_type: "multiple_choice".to_owned(),
            prompt: "?".to_owned(),
            choices: None,
            anchor: None,
        };
        assert!(build_question_input(&mc_no_choices, "P", "T", "d.md", "r", "b").is_none());

        let empty_prompt = QuestionManifestEntry {
            question_type: "prompt".to_owned(),
            prompt: "   ".to_owned(),
            choices: None,
            anchor: None,
        };
        assert!(build_question_input(&empty_prompt, "P", "T", "d.md", "r", "b").is_none());
    }

    #[test]
    fn build_question_input_serializes_choices() {
        let mc = QuestionManifestEntry {
            question_type: "multiple_choice".to_owned(),
            prompt: "pick".to_owned(),
            choices: Some(vec!["a".to_owned(), "b".to_owned()]),
            anchor: Some("sec".to_owned()),
        };
        let input = build_question_input(&mc, "P", "T", "d.md", "r", "b").unwrap();
        assert_eq!(input.choice_options.as_deref(), Some(r#"["a","b"]"#));
        assert_eq!(input.source_anchor.as_deref(), Some("sec"));
    }

    #[test]
    fn parse_followups_block_handles_fenced_json() {
        let text = "Some summary.\n\nFOLLOWUPS:\n```json\n[\n  {\"proposed_name\": \"Wire retries\", \"proposed_description\": \"add backoff\", \"proposed_effort\": \"small\"}\n]\n```\n";
        let entries = parse_followups_block(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].proposed_name, "Wire retries");
        assert_eq!(entries[0].proposed_effort.as_deref(), Some("small"));
    }

    #[test]
    fn parse_followups_block_handles_unfenced_array() {
        let text = "FOLLOWUPS: [{\"proposed_name\": \"X\", \"proposed_description\": \"y\"}]";
        let entries = parse_followups_block(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].proposed_name, "X");
    }

    #[test]
    fn parse_followups_block_uses_last_sentinel() {
        // An earlier mention (e.g. echoing instructions) is ignored in favour
        // of the final, real block.
        let text = "I will emit FOLLOWUPS: later.\n\nFOLLOWUPS:\n[{\"proposed_name\": \"Real\", \"proposed_description\": \"d\"}]";
        let entries = parse_followups_block(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].proposed_name, "Real");
    }

    #[test]
    fn parse_followups_block_empty_without_sentinel() {
        assert!(parse_followups_block("no block here [1,2,3]").is_empty());
    }

    #[test]
    fn extract_balanced_array_respects_strings() {
        // A `]` inside a string literal must not terminate the array.
        let s = r#"prefix [{"k": "a]b"}] suffix"#;
        assert_eq!(extract_balanced_array(s).as_deref(), Some(r#"[{"k": "a]b"}]"#));
    }

    #[test]
    fn build_followup_input_drops_unknown_work_kind() {
        let entry = FollowupEntry {
            proposed_name: "Do X".to_owned(),
            proposed_description: Some("desc".to_owned()),
            proposed_effort: None,
            proposed_work_kind: Some("epic".to_owned()),
            rationale: None,
        };
        let input = build_followup_input(&entry, "T1", "E1").unwrap();
        assert_eq!(input.proposed_work_kind, None);
        assert_eq!(input.proposed_name.as_deref(), Some("Do X"));

        let nameless = FollowupEntry {
            proposed_name: "  ".to_owned(),
            proposed_description: None,
            proposed_effort: None,
            proposed_work_kind: None,
            rationale: None,
        };
        assert!(build_followup_input(&nameless, "T1", "E1").is_none());
    }

    // ── Backstop helper tests ────────────────────────────────────────────────

    #[test]
    fn is_risks_heading_matches_variants() {
        assert!(is_risks_heading("risks / open questions"));
        assert!(is_risks_heading("risks/open questions"));
        assert!(is_risks_heading("open questions"));
        assert!(is_risks_heading("open question"));
        assert!(is_risks_heading("risks and open questions"));
        assert!(is_risks_heading("risks"));
        assert!(is_risks_heading("open questions and risks"));
    }

    #[test]
    fn is_risks_heading_rejects_unrelated() {
        assert!(!is_risks_heading("introduction"));
        assert!(!is_risks_heading("implementation"));
        assert!(!is_risks_heading("alternatives considered"));
        assert!(!is_risks_heading("chosen approach"));
    }

    #[test]
    fn extract_risks_section_numbered_list() {
        let doc = "# Intro\n\nSome text.\n\n## Risks / open questions\n\n\
                   1. **OQ1** — First question text.\n\
                   2. **OQ2** — Second question text.\n\n\
                   ## Next section\n\nOther content.";
        let items = extract_risks_section_items(doc);
        assert_eq!(items.len(), 2);
        assert!(items[0].contains("First question text"), "got: {}", items[0]);
        assert!(items[1].contains("Second question text"), "got: {}", items[1]);
        // Bold markers should be stripped.
        assert!(!items[0].contains("**"), "bold not stripped: {}", items[0]);
    }

    #[test]
    fn extract_risks_section_bulleted_list() {
        let doc = "# Doc\n\n## Open Questions\n\n\
                   - Should we gate behind a flag?\n\
                   * What is the table count?\n\
                   + Another one.\n\n\
                   ## Done\n";
        let items = extract_risks_section_items(doc);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "Should we gate behind a flag?");
        assert_eq!(items[1], "What is the table count?");
        assert_eq!(items[2], "Another one.");
    }

    #[test]
    fn extract_risks_section_no_match_returns_empty() {
        let doc = "# Intro\n\nNothing relevant here.\n\n## Implementation\n\nCode.";
        assert!(extract_risks_section_items(doc).is_empty());
    }

    #[test]
    fn extract_risks_section_stops_at_next_heading() {
        let doc = "## Risks / open questions\n\n1. Only this.\n\n## Next\n\n1. Not this.";
        let items = extract_risks_section_items(doc);
        assert_eq!(items.len(), 1);
        assert!(items[0].contains("Only this"));
    }

    #[test]
    fn strip_numbered_list_prefix_handles_variants() {
        assert_eq!(strip_numbered_list_prefix("1. text"), Some("text"));
        assert_eq!(strip_numbered_list_prefix("12. text"), Some("text"));
        assert_eq!(strip_numbered_list_prefix("3) text"), Some("text"));
        assert_eq!(strip_numbered_list_prefix("not a list"), None);
        assert_eq!(strip_numbered_list_prefix("1 text"), None);
    }

    #[test]
    fn strip_markdown_bold_removes_markers() {
        assert_eq!(strip_markdown_bold("**bold** text"), "bold text");
        assert_eq!(strip_markdown_bold("__also bold__"), "also bold");
        assert_eq!(strip_markdown_bold("plain text"), "plain text");
        assert_eq!(strip_markdown_bold("**OQ1** — First."), "OQ1 — First.");
    }

    #[test]
    fn build_followups_supervisor_prompt_contains_tail() {
        let tail = "some transcript content here";
        let prompt = build_followups_supervisor_prompt(tail);
        assert!(prompt.contains(tail));
        assert!(prompt.contains("JSON array"));
        assert!(prompt.contains("proposed_name"));
    }
}
