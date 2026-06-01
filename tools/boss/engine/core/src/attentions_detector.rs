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
//! concern and are intentionally not part of this structured-first path.

use std::process::Stdio;

use boss_protocol::{Attention, AttentionGroup, CreateAttentionInput};
use boss_transcript_markdown::{TranscriptEventKind, parse_transcript};
use serde::Deserialize;
use tokio::process::Command;

use crate::design_detector;
use crate::work::WorkDb;

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
    let rest = pr_url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
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
}
