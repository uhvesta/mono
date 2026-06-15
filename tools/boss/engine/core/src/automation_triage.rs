//! Automation triage execution: preamble rendering, decision-marker
//! parsing, and the real [`TriageDispatcher`] (Maint task 6).
//!
//! The scheduler (Maint task 5) decides *when* an automation fires and calls
//! [`crate::automation_scheduler::TriageDispatcher::dispatch_triage`] through
//! a seam. This module supplies the real implementation,
//! [`EngineTriageDispatcher`], which creates a `ready`
//! [`boss_protocol::EXECUTION_KIND_AUTOMATION_TRIAGE`] work_execution bound to
//! the automation and kicks the coordinator. From there the execution flows
//! through the *normal* dispatch pipeline (cube lease → worker pane), routed
//! to the automations pool by `kind`, with one difference at each end:
//!
//! - **Spawn:** the runner renders [`render_triage_preamble`] instead of the
//!   ordinary work-item prompt (the worker is a *triage* agent, not an
//!   implementer).
//! - **Stop:** the completion handler parses the worker's final message with
//!   [`parse_triage_decision`] and finalises the matching `automation_runs`
//!   row, rather than running PR detection.
//!
//! ## The marker protocol (design Risk #3)
//!
//! The whole value of phase 1 hinges on the triage agent reliably emitting
//! *exactly one* decision marker and **not** doing the work itself. The
//! preamble states the contract; the marker parser enforces "exactly one";
//! and the transactional cap re-check at `boss task create --automation`
//! (see [`crate::work::WorkDb::create_automation_task`]) is the backstop
//! against a misbehaving agent fanning out.

use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::Automation;

use crate::automation_scheduler::{TriageDispatch, TriageDispatcher};
use crate::work::WorkDb;

/// One decision the triage agent can reach, parsed from its final message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// `automation: task <id>` — the agent created a task with the given
    /// (friendly or canonical) id. The detector verifies the id resolves to a
    /// task whose `source_automation_id` is this automation before trusting it.
    ProducedTask(String),
    /// `automation: skip — <reason>` — the agent decided nothing actionable
    /// exists right now. An explicit, agent-authored no-op.
    Skip(String),
    /// No marker at all — the worker errored, was reaped, or simply never
    /// reached a decision. Treated as a transient/ambiguous failure (the run
    /// is left `failed_will_retry`), never as a skip.
    NoDecision,
    /// More than one marker line — the contract was violated. Treated like
    /// `NoDecision`: we refuse to guess which decision the agent meant.
    Ambiguous(usize),
}

/// Compose the per-automation triage preamble (design §"Phase 1 — Triage").
///
/// `product_name` is the display name of the automation's product. The
/// `--automation` selector embedded in the create command is the **canonical**
/// automation id so the agent's `boss task create` resolves unambiguously
/// without needing a `--product` flag.
pub fn render_triage_preamble(automation: &Automation, product_name: &str) -> String {
    let a_id = automation
        .short_id
        .map(|n| format!("A{n}"))
        .unwrap_or_else(|| automation.id.clone());
    let create_cmd = format!(
        "boss task create --automation {} --name \"<concise title>\" --description \"<what to do>\"",
        automation.id
    );
    format!(
        "You are a maintenance **triage** agent for automation `{a_id}` on product \
\"{product_name}\". Your session cwd is already a fresh checkout of this product's \
repository.\n\n\
Standing instruction:\n\n> {instruction}\n\n\
Decide whether a **single, concrete, actionable** task can be derived from this \
instruction **right now** in this repository. Investigate the repo as needed to \
make that call. You are explicitly allowed to conclude that nothing appropriate \
exists — that is a normal, expected outcome on most runs.\n\n\
## You MUST end this run with exactly one decision marker\n\n\
Your final message must end with **exactly one** of these two lines, and nothing \
after it:\n\n\
- **If there is work to do** — create exactly **one** task, then emit:\n\n\
  ```\n  {create_cmd}\n  ```\n\n\
  The command prints the new task id (e.g. `T42`). Then end your final message with \
the line:\n\n\
  ```\n  automation: task T42\n  ```\n\n\
- **If there is nothing appropriate to do right now**, end your final message with:\n\n\
  ```\n  automation: skip — <one-line reason>\n  ```\n\n\
## Single-shot mandate — no sub-agents, no deferral\n\n\
This run is **single-shot**: the investigation AND the decision marker must both \
happen within this session. The session ends the moment you stop responding.\n\n\
- **Do NOT use the `Agent` tool.** Spawning a sub-agent provides no resume \
mechanism — the session will hang waiting for a result that never returns.\n\
- **Do NOT end any turn with deferred intent** such as \"I'll create the task \
next\", \"Let me investigate further\", or \"I'll wait for the agent to finish\". \
If you state an intent like \"Let me create the task\", you must follow through \
immediately in that same turn — do not stop before you do.\n\
- **Do NOT wait for any external process or event.** All investigation must \
happen inline using read-only tool calls (`grep`/`find`/`cat`, `Bash`, `Read`, \
`WebSearch`). Finish the investigation before you make your decision.\n\
- **If you create a task** with `boss task create --automation`, emit the \
`automation: task <id>` marker **in the same response**, immediately after the \
tool call returns with the task id. Do not stop between the tool call and the \
marker.\n\n\
## Hard guardrails\n\n\
- **Do NOT do the work yourself.** Do not edit files, do not commit, do not open a \
PR. A separate worker executes the task you create. Your only deliverable is the \
decision marker (and, if applicable, the one `boss task create --automation` call).\n\
- **Create at most one task.** The automation enforces an open-task cap; a second \
`boss task create --automation` call in this run will be rejected.\n\
- **Emit exactly one marker line**, as the very last line of your final message. \
Zero markers (or more than one) is treated as an inconclusive run and retried — it \
is NOT a skip.\n",
        a_id = a_id,
        product_name = product_name,
        instruction = automation.standing_instruction.trim(),
        create_cmd = create_cmd,
    )
}

/// Render the CLAUDE.md for a triage worker ([`crate::worker_setup::WorkerKind::Triage`]).
///
/// A triage worker is **not** an implementer: its deliverable is a decision
/// marker, never a pull request. It therefore MUST NOT receive the standard
/// implementation-worker CLAUDE.md (rendered by
/// [`crate::worker_setup::render_claude_md`] for
/// [`crate::worker_setup::WorkerKind::Standard`]), which states "a task is not
/// complete until a PR exists / PR creation is your terminal act / print the
/// PR URL as the last line of your final response". Those instructions
/// directly contradict the triage marker contract in
/// [`render_triage_preamble`]; a worker caught between the two ends its run
/// with a PR-shaped summary (or stops because `jj diff` is empty) and never
/// emits a marker, so the run is finalised `failed_will_retry` /
/// "triage ended without a decision marker". This CLAUDE.md restates the
/// marker contract and the no-work / no-PR posture, and omits the PR-delivery
/// mandate entirely (the [`crate::worker_setup::triage_deny_rules`] denylist is
/// the suspenders to this belt).
pub fn render_triage_claude_md(lease_id: &str) -> String {
    format!(
        "# Boss triage rules\n\
         \n\
         You are running inside a Boss-managed **triage** session. The engine\n\
         spawned you in a leased cube workspace to decide whether a single,\n\
         concrete, actionable task can be derived from an automation's standing\n\
         instruction right now in this repository.\n\
         \n\
         ## Triage mandate (HARD CONSTRAINT)\n\
         \n\
         **There is NO pull-request deliverable for a triage run.** Your only\n\
         deliverable is a single decision marker.\n\
         \n\
         Your final message MUST end with **exactly one** of these two lines,\n\
         and nothing after it:\n\
         \n\
         - `automation: task <id>` — after creating **exactly one** task with\n\
           `boss task create --automation <automation-id> --name \"…\" --description \"…\"`\n\
           (the command prints the new task id, e.g. `T42`).\n\
         - `automation: skip — <one-line reason>` — when nothing appropriate\n\
           exists right now (a normal, expected outcome on most runs).\n\
         \n\
         Zero markers, or more than one, is treated as an inconclusive run and\n\
         retried — it is NOT a skip. Concluding \"nothing to do\" is a `skip`,\n\
         never a silent end.\n\
         \n\
         ## Single-shot mandate — no sub-agents, no deferral\n\
         \n\
         This run is **single-shot**: investigation AND the decision marker must\n\
         both happen within this session. The session ends the moment you stop.\n\
         \n\
         - **Do NOT use the `Agent` tool.** Sub-agents provide no resume\n\
           mechanism — spawning one will hang the session indefinitely.\n\
         - **Do NOT defer to a later turn.** If you say \"I'll create the task\n\
           next\" or \"Let me wait for the agent\", you must complete that action\n\
           immediately in the same turn — the session will NOT give you another.\n\
         - **If you run `boss task create --automation`**, emit the\n\
           `automation: task <id>` marker in the **same response**, right after\n\
           the tool call returns the task id. Do not stop between the two.\n\
         \n\
         ## Do NOT do the work (tool calls for these are denied)\n\
         \n\
         A separate worker executes the task you create. You only decide and\n\
         emit the marker. Forbidden here:\n\
         \n\
         - Editing or writing any file (`Edit`, `Write`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, merging, closing, editing, or commenting on a PR\n\
           (`gh pr create/merge/close/edit/comment/review`) or running\n\
           `cube pr create`/`cube pr update`.\n\
         - Filing or updating GitHub issues.\n\
         \n\
         Do NOT create a PR, do NOT push a branch, and do NOT print a PR URL —\n\
         none of that applies to a triage run. Investigate read-only (`grep`,\n\
         `find`, `cat`, `jj log`/`show`/`diff`, etc.), then create at most one\n\
         task and emit your marker.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside your workspace. Other workspaces\n\
           belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
           Never read, write, or touch it.\n\
           `bossctl` is coordinator-only.\n\
         \n\
         ## Coordinator\n\
         \n\
         The coordinator may probe this session between turns. Treat probes\n\
         as questions from a human reviewer — short, specific answers.\n",
        lease = lease_id,
    )
}

/// Parse the triage agent's final assistant message into a [`TriageDecision`].
///
/// Scans every line for a decision marker (`automation: task <id>` /
/// `automation: skip — <reason>`) and enforces the "exactly one" contract:
///
/// - exactly one valid marker → that decision,
/// - zero markers → [`TriageDecision::NoDecision`],
/// - two or more markers → [`TriageDecision::Ambiguous`].
///
/// Matching is lenient on case and on the skip separator (em-dash `—`, hyphen
/// `-`, or colon `:` all accepted) but strict on the `automation:` prefix and
/// on the `task` / `skip` keyword having a word boundary, so prose that merely
/// *mentions* the protocol does not trip it.
pub fn parse_triage_decision(final_message: &str) -> TriageDecision {
    let markers: Vec<TriageDecision> = final_message.lines().filter_map(parse_marker_line).collect();
    match markers.len() {
        0 => TriageDecision::NoDecision,
        1 => markers.into_iter().next().unwrap(),
        n => TriageDecision::Ambiguous(n),
    }
}

/// Parse a single line into a marker, or `None` if it is not one. A `task`
/// marker with an empty id is rejected (returns `None`) — an explicit skip
/// with an empty reason is still a valid `Skip`.
fn parse_marker_line(line: &str) -> Option<TriageDecision> {
    let after_prefix = strip_ci_prefix(line.trim(), "automation:")?.trim_start();

    if let Some(rest) = strip_keyword(after_prefix, "task") {
        let id = rest.trim();
        if id.is_empty() {
            return None;
        }
        return Some(TriageDecision::ProducedTask(id.to_owned()));
    }
    if let Some(rest) = strip_keyword(after_prefix, "skip") {
        let reason = rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == '-' || c == ':')
            .trim();
        return Some(TriageDecision::Skip(reason.to_owned()));
    }
    None
}

/// Case-insensitively strip `prefix` from the start of `s`, returning the
/// remainder. ASCII-only prefixes keep the byte/char-boundary slice safe.
fn strip_ci_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (sb, pb) = (s.as_bytes(), prefix.as_bytes());
    if sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb) {
        Some(&s[pb.len()..])
    } else {
        None
    }
}

/// Like [`strip_ci_prefix`] but requires a trailing word boundary so `task`
/// matches `task T1` but not `taskforce`.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = strip_ci_prefix(s, keyword)?;
    if rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric() && c != '_') {
        Some(rest)
    } else {
        None
    }
}

/// The real [`TriageDispatcher`]: creates an `automation_triage` execution and
/// kicks the coordinator so its normal drain picks the row up.
///
/// `kick` is a thin closure over `ExecutionCoordinator::kick`, mirroring how
/// the other sweepers (`dep_unblock_sweep`) re-enter the scheduler — it keeps
/// this module free of a hard dependency on the coordinator type.
pub struct EngineTriageDispatcher {
    work_db: Arc<WorkDb>,
    kick: Arc<dyn Fn() + Send + Sync>,
}

impl EngineTriageDispatcher {
    pub fn new(work_db: Arc<WorkDb>, kick: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { work_db, kick }
    }

    /// Resolve the repo the triage worker should lease: the automation's
    /// explicit `repo_remote_url` override, else the product's primary repo.
    /// `None` when neither is available (a genuinely unrunnable automation).
    fn resolve_repo(&self, automation: &Automation) -> Option<String> {
        if let Some(repo) = automation.repo_remote_url.clone() {
            return Some(repo);
        }
        self.work_db
            .get_product(&automation.product_id)
            .ok()
            .flatten()
            .and_then(|p| p.repo_remote_url)
    }

    /// Shared fire path used by both the scheduler seam and the manual
    /// `boss automation run` verb: resolve repo, create the triage execution,
    /// kick the coordinator.
    pub fn fire(&self, automation: &Automation) -> TriageDispatch {
        let Some(repo) = self.resolve_repo(automation) else {
            return TriageDispatch::TransientFailure {
                detail: format!(
                    "automation {} has no repo and its product has no primary repo; \
                     cannot lease a workspace",
                    automation.id
                ),
            };
        };
        match self.work_db.create_automation_triage_execution(&automation.id, &repo) {
            Ok(execution) => {
                (self.kick)();
                TriageDispatch::Dispatched {
                    execution_id: execution.id,
                }
            }
            Err(err) => TriageDispatch::TransientFailure {
                detail: format!("failed to create triage execution: {err:#}"),
            },
        }
    }
}

#[async_trait]
impl TriageDispatcher for EngineTriageDispatcher {
    async fn dispatch_triage(&self, automation: &Automation, _scheduled_for_epoch: i64) -> TriageDispatch {
        self.fire(automation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_task_marker() {
        let msg = "Found a clear win.\n\nautomation: task T42\n";
        assert_eq!(
            parse_triage_decision(msg),
            TriageDecision::ProducedTask("T42".to_owned())
        );
    }

    #[test]
    fn parses_canonical_task_id() {
        assert_eq!(
            parse_triage_decision("automation: task task_018abc"),
            TriageDecision::ProducedTask("task_018abc".to_owned())
        );
    }

    #[test]
    fn parses_skip_with_em_dash() {
        assert_eq!(
            parse_triage_decision("automation: skip — no clippy warnings today"),
            TriageDecision::Skip("no clippy warnings today".to_owned())
        );
    }

    #[test]
    fn parses_skip_with_hyphen_or_colon() {
        assert_eq!(
            parse_triage_decision("automation: skip - nothing to do"),
            TriageDecision::Skip("nothing to do".to_owned())
        );
        assert_eq!(
            parse_triage_decision("automation: skip: already clean"),
            TriageDecision::Skip("already clean".to_owned())
        );
    }

    #[test]
    fn case_insensitive_prefix() {
        assert_eq!(
            parse_triage_decision("Automation: Task T7"),
            TriageDecision::ProducedTask("T7".to_owned())
        );
    }

    #[test]
    fn zero_markers_is_no_decision() {
        assert_eq!(
            parse_triage_decision("I looked around but did not finish."),
            TriageDecision::NoDecision
        );
    }

    #[test]
    fn two_markers_is_ambiguous() {
        let msg = "automation: task T1\nautomation: skip — changed my mind";
        assert_eq!(parse_triage_decision(msg), TriageDecision::Ambiguous(2));
    }

    #[test]
    fn prose_mentioning_protocol_does_not_match() {
        // A word like "taskforce" must not be read as a `task` marker, and a
        // sentence describing the protocol without the exact prefix is inert.
        assert_eq!(
            parse_triage_decision("automation: taskforce assembled"),
            TriageDecision::NoDecision
        );
        assert_eq!(
            parse_triage_decision("I will emit automation markers when done."),
            TriageDecision::NoDecision
        );
    }

    #[test]
    fn empty_task_id_is_not_a_marker() {
        assert_eq!(parse_triage_decision("automation: task   "), TriageDecision::NoDecision);
    }

    #[test]
    fn skip_with_empty_reason_is_still_a_skip() {
        assert_eq!(
            parse_triage_decision("automation: skip"),
            TriageDecision::Skip(String::new())
        );
    }

    #[test]
    fn leading_and_trailing_whitespace_on_marker_line_tolerated() {
        assert_eq!(
            parse_triage_decision("   automation: task T9   "),
            TriageDecision::ProducedTask("T9".to_owned())
        );
    }

    #[test]
    fn triage_claude_md_restates_marker_contract_and_omits_pr_mandate() {
        let md = render_triage_claude_md("lease_abc");
        // The lease id is surfaced so a confused worker can describe itself.
        assert!(md.contains("lease_abc"));
        // Restates the marker contract (the whole point of the triage run).
        assert!(md.contains("automation: task"));
        assert!(md.contains("automation: skip"));
        assert!(
            md.contains("exactly one"),
            "triage CLAUDE.md must restate the exactly-one-marker contract",
        );
        // Must NOT carry the implementation worker's PR-delivery mandate — that
        // contradiction is the root cause of "triage ended without a decision
        // marker" (the worker chases a PR and never emits the marker).
        assert!(
            !md.contains("Pull requests are the deliverable"),
            "triage CLAUDE.md must not include the standard PR-required reminder",
        );
        assert!(
            !md.contains("A task is not complete until a PR exists"),
            "triage CLAUDE.md must not include the implementation PR mandate",
        );
        assert!(
            !md.contains("PR creation is your terminal act"),
            "triage CLAUDE.md must not tell the worker its terminal act is a PR",
        );
        assert!(
            !md.contains("Print the PR URL"),
            "triage CLAUDE.md must not instruct the worker to print a PR URL",
        );
        // States the no-PR posture explicitly.
        assert!(md.contains("no pull-request deliverable") || md.contains("NO pull-request deliverable"));
    }

    #[test]
    fn triage_claude_md_forbids_sub_agents_and_deferral() {
        let md = render_triage_claude_md("lease_xyz");
        // Must explicitly name the Agent tool and explain why it is forbidden
        // (the hang mode: no resume mechanism once a sub-agent is spawned).
        assert!(
            md.contains("Agent"),
            "triage CLAUDE.md must mention the Agent tool to tell the worker not to use it",
        );
        // Must warn against deferring intent to a later turn.
        assert!(
            md.contains("defer") || md.contains("deferral") || md.contains("later turn"),
            "triage CLAUDE.md must warn against deferring intent to a later turn",
        );
        // Must tell the worker to emit the marker in the same response as the
        // task-create tool call.
        assert!(
            md.contains("same response") || md.contains("same turn"),
            "triage CLAUDE.md must instruct the worker to emit the marker in the same response as the tool call",
        );
    }

    #[test]
    fn preamble_forbids_sub_agents_and_deferral() {
        let automation = Automation::builder()
            .id("auto_abc")
            .short_id(1i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(&automation, "My Product");
        // Must explicitly name the Agent tool and explain the hang risk.
        assert!(
            preamble.contains("Agent"),
            "preamble must name the Agent tool to tell the worker not to use it",
        );
        // Must name the failure mode (sub-agent hang) so the worker understands why.
        assert!(
            preamble.contains("sub-agent") || preamble.contains("sub agent"),
            "preamble must mention sub-agents",
        );
        // Must require the marker to be emitted in the same response as the task
        // creation — the premature-end failure mode in the field evidence.
        assert!(
            preamble.contains("same response") || preamble.contains("same turn"),
            "preamble must instruct the worker to emit the marker in the same response as the tool call",
        );
        // Must warn against deferred intent.
        assert!(
            preamble.to_lowercase().contains("defer") || preamble.contains("later turn"),
            "preamble must warn against deferring intent to a later turn",
        );
    }

    #[test]
    fn preamble_includes_contract_and_canonical_selector() {
        let automation = Automation::builder()
            .id("auto_123")
            .short_id(3)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(&automation, "My Product");
        assert!(preamble.contains("triage"));
        assert!(preamble.contains("A3"));
        assert!(preamble.contains("My Product"));
        assert!(preamble.contains("fix any clippy warnings"));
        // Canonical selector so the agent's create resolves without --product.
        assert!(preamble.contains("--automation auto_123"));
        assert!(preamble.contains("automation: task"));
        assert!(preamble.contains("automation: skip"));
        assert!(preamble.contains("Do NOT do the work"));
    }

    /// Regression test: when the triage agent calls `boss task create` the
    /// decision marker appears in the SECOND assistant turn (after the tool
    /// result). The previous `iter().rev().find_map(AssistantText)` approach
    /// returned only the last AssistantText event; if the Stop hook fires
    /// before that post-tool turn is fully flushed to disk, the engine read
    /// the pre-tool analysis text (no marker) instead, recording
    /// `failed_will_retry`. The fix concatenates ALL AssistantText turns so
    /// the marker is detected regardless of which turn contains it.
    #[test]
    fn marker_detected_from_concatenated_multi_turn_transcript() {
        use boss_transcript_markdown::{TranscriptEventKind, parse_transcript};

        // Simulate the JSONL transcript for a task-creating triage run:
        //   Turn 1: analysis prose + boss task create tool call
        //   (tool result from the tool)
        //   Turn 2: post-tool summary with the decision marker
        let jsonl = concat!(
            // Turn 1: analysis + tool_use
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I found work to do. Let me create a task."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"boss task create --automation auto_xxx --name \"Fix tests\""}}]}}"#,
            "\n",
            // Tool result
            r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"Created task T1330"}],"isError":false}"#,
            "\n",
            // Turn 2: post-tool marker (the critical turn)
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Created task T1330.\n\nautomation: task T1330"}]}}"#,
            "\n",
        );

        let events = parse_transcript(jsonl);

        // Collect all AssistantText events (the fix's approach).
        let all_text: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::AssistantText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();

        // There are two assistant text turns.
        assert_eq!(all_text.len(), 2, "should have two assistant text turns");

        // The OLD code: find the last AssistantText.
        // When the post-tool turn IS present, even the old code would work.
        // The bug manifested when the post-tool turn was MISSING from the
        // transcript (timing race). Simulate that by taking only the first turn:
        let only_pre_tool = &all_text[..1];
        let pre_tool_decision = parse_triage_decision(&only_pre_tool[0]);
        assert_eq!(
            pre_tool_decision,
            TriageDecision::NoDecision,
            "pre-tool analysis text has no marker — this is what the old code saw when Turn 2 was missing"
        );

        // The NEW code: join all turns and parse the combined text.
        let combined = all_text.join("\n");
        let decision = parse_triage_decision(&combined);
        assert_eq!(
            decision,
            TriageDecision::ProducedTask("T1330".to_owned()),
            "concatenating all turns finds the marker in the post-tool turn"
        );
    }
}
