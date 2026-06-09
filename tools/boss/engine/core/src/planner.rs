//! The Planner — a reusable LLM "mini-coordinator".
//!
//! Given a merged design doc plus project/product context, the Planner
//! proposes the project's implementation task graph: the tasks to create
//! (with effort levels and kinds) and the dependency edges that let work
//! proceed in parallel. It is the automated stand-in for a human
//! coordinator who would otherwise read the doc by hand and type out
//! `boss task create` / `boss task depend add` calls.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! (project P783) §2 "The Planner". This module is task 3 of that design.
//!
//! ## Pure transform, no writes
//!
//! [`Planner::plan`] takes the typed [`PlannerInput`] (defined in
//! `boss-protocol`) and returns a typed [`PlannerOutput`]. It performs no
//! writes and has no knowledge of the trigger that invoked it — the
//! deterministic *Materializer* (a sibling task) is the only thing that
//! writes rows. Keeping the Planner a pure prose-to-typed-graph transform
//! is what makes the auto-populate feature testable, idempotent, and safe.
//!
//! ## Reuses the `live_status` Anthropic substrate
//!
//! The engine already POSTs to `https://api.anthropic.com/v1/messages` via
//! a shared `reqwest` client in [`crate::live_status`], returning a *typed
//! outcome* so callers can distinguish "no API key" from "model 429" from
//! "succeeded". The Planner reuses that exact shape: a process-wide client,
//! the pinned API version, and a typed [`PlannerOutcome`] rather than an
//! `anyhow::Result` that would erase the distinction the caller (the
//! Populator, a sibling task) needs to record the right `planner_runs`
//! outcome. The design names the entry point `Planner::plan(PlannerInput)
//! -> Result<PlannerOutput>`; we return the richer [`PlannerOutcome`] enum
//! to honour the design's adjacent requirement to "return typed outcomes
//! (`NoApiKey`, `ApiError`, success)".
//!
//! ## Structured output is enforced, not requested
//!
//! The call forces a single tool call (`tool_choice: {type: "tool"}`) whose
//! `input_schema` is [`planner_output_schema`]. The model is therefore
//! obligated to emit the [`PlannerOutput`] shape, which we deserialise
//! directly into the Rust type — a deserialisation failure is a validation
//! failure ([`PlannerOutcome::InvalidOutput`]), never a parse-and-hope over
//! free-form markdown.
//!
//! ## Bounded model / effort / timeout
//!
//! Planning quality matters and the call is infrequent (once per project),
//! so the Planner defaults to a strong model (Opus) rather than the Haiku
//! that `live_status` uses for its cheap one-liner. The model, effort,
//! `max_tokens`, timeout, and retry count are all single constants, tunable
//! without a schema change (design R5).

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

use boss_protocol::{PlannerInput, PlannerOutput, planner_output_schema};

/// Anthropic Messages API endpoint. Hard-coded; matches
/// [`crate::live_status`] and [`crate::pane_summary`].
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// The model the Planner runs on. A direct API call needs a concrete model
/// id (the `--model` family aliases used for worker dispatch are resolved by
/// the `claude` CLI, not the Messages API), so this is pinned rather than an
/// alias. Opus is deliberate: planning quality matters and the call is
/// infrequent (once per project), unlike the Haiku one-liner in
/// [`crate::live_status`]. Tunable here without a schema change (design R5).
pub const PLANNER_MODEL: &str = "claude-opus-4-8";

/// `output_config.effort` for the planning call (design "bound … effort").
/// `high` is the recommended minimum for intelligence-sensitive work;
/// extracting a typed task graph from prose is intelligence-sensitive but
/// bounded, so we do not spend up at `xhigh`/`max`.
pub const PLANNER_EFFORT: &str = "high";

/// Output ceiling. A breakdown of up to ~30 tasks — each with a description
/// plus its `[effort-classification]` line — plus the edge set and notes
/// fits comfortably here, and staying at/under ~16K keeps the non-streaming
/// request under the SDK/HTTP timeout envelope.
pub const PLANNER_MAX_TOKENS: u32 = 16_384;

/// Wall-clock budget for one planning round trip. A high-effort Opus call
/// over a full design doc is far slower than the `live_status` one-liner, so
/// this is generous — but still bounded so a wedged call cannot hang the
/// caller indefinitely (design "bound … timeout").
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(180);

/// Total attempts per [`Planner::plan`] call: the design says "retry once,
/// then fail safe", i.e. two attempts.
pub const PLANNER_ATTEMPTS: usize = 2;

/// Name of the forced tool whose `input_schema` is [`planner_output_schema`].
/// The model must call exactly this tool; its `input` is the structured
/// [`PlannerOutput`].
pub const TOOL_NAME: &str = "emit_task_graph";

/// One-line tool description shown to the model alongside the schema.
const TOOL_DESCRIPTION: &str =
    "Emit the proposed implementation task graph extracted from the design \
     document: the tasks to create (with kind and effort), the dependency \
     edges between them by handle, the confidence, whether a breakdown was \
     found, the per-task [effort-classification] audit lines, and a notes \
     rationale.";

/// Distinguishable outcomes for one planning call. Mirrors
/// [`crate::live_status::SummarizerOutcome`]: the caller (the Populator)
/// needs to tell "no API key" from "model 429" from "succeeded" so it can
/// record the right `planner_runs.outcome` and surface the right attention
/// item. A bare `anyhow::Result<PlannerOutput>` would erase that.
#[derive(Debug, Clone)]
pub enum PlannerOutcome {
    /// The model returned a schema-valid [`PlannerOutput`].
    Success(PlannerOutput),
    /// No `ANTHROPIC_API_KEY` was configured on the engine. The feature
    /// degrades to "design pointer set, tasks not auto-created" and the
    /// caller surfaces an attention item asking the operator to configure
    /// the key — exactly as `live_status` degrades.
    NoApiKey,
    /// Anthropic returned a non-2xx response. `status` is the numeric code
    /// (e.g. 401, 429, 529); `snippet` is the first ~200 chars of the body.
    ApiError { status: u16, snippet: String },
    /// The HTTP client failed before/while getting a response (timeout, TLS,
    /// DNS, connection reset), or the response body could not be decoded.
    Transport(String),
    /// A response arrived but the model did not call [`TOOL_NAME`], or its
    /// tool input did not deserialise into [`PlannerOutput`]. Treated as a
    /// validation failure, not a transport error.
    InvalidOutput(String),
}

impl PlannerOutcome {
    /// Short tag for logs and the `planner_runs` audit row.
    pub fn tag(&self) -> &'static str {
        match self {
            PlannerOutcome::Success(_) => "success",
            PlannerOutcome::NoApiKey => "no_api_key",
            PlannerOutcome::ApiError { .. } => "api_error",
            PlannerOutcome::Transport(_) => "transport_error",
            PlannerOutcome::InvalidOutput(_) => "invalid_output",
        }
    }

    /// Human-readable detail for logs and the operator-facing audit record.
    pub fn detail(&self) -> String {
        match self {
            PlannerOutcome::Success(out) => {
                format!(
                    "{} task(s), {} edge(s), confidence={}, breakdown_found={}",
                    out.tasks.len(),
                    out.edges.len(),
                    out.confidence,
                    out.breakdown_found,
                )
            }
            PlannerOutcome::NoApiKey => {
                "ANTHROPIC_API_KEY not configured on the engine".to_owned()
            }
            PlannerOutcome::ApiError { status, snippet } => {
                format!("anthropic returned {status}: {snippet}")
            }
            PlannerOutcome::Transport(err) => err.clone(),
            PlannerOutcome::InvalidOutput(err) => err.clone(),
        }
    }
}

/// The Planner. A zero-sized entry point so callers write the
/// `Planner::plan(..)` shape the design names; the Planner holds no state
/// (it is a pure transform).
pub struct Planner;

impl Planner {
    /// Plan the implementation task graph for one project from its merged
    /// design doc.
    ///
    /// `api_key` is passed in (not read from config here) so the Planner
    /// stays a pure transform with no config/DB dependency — the caller
    /// sources it from `Config::anthropic_api_key`, mirroring
    /// [`crate::live_status::summarize_transcript`]. A `None` key short-
    /// circuits to [`PlannerOutcome::NoApiKey`] without a network call.
    ///
    /// On a transient failure (transport, decode, non-2xx, or output that
    /// fails schema validation) the call is retried once before failing
    /// safe with the *last* error mapped into a [`PlannerOutcome`].
    pub async fn plan(api_key: Option<&str>, input: &PlannerInput) -> PlannerOutcome {
        match api_key {
            None => {
                tracing::error!(
                    "planner: skipped — ANTHROPIC_API_KEY not configured",
                );
                PlannerOutcome::NoApiKey
            }
            Some(key) => plan_with_url(ANTHROPIC_MESSAGES_URL, key, input).await,
        }
    }
}

/// Core of [`Planner::plan`] with the endpoint URL injected so tests can
/// drive it against a mock server. Builds the request once and runs up to
/// [`PLANNER_ATTEMPTS`] attempts.
async fn plan_with_url(url: &str, api_key: &str, input: &PlannerInput) -> PlannerOutcome {
    let body = build_request_body(input);
    let mut last: Option<PlannerCallError> = None;
    for attempt in 1..=PLANNER_ATTEMPTS {
        match call_anthropic(url, api_key, &body).await {
            Ok(output) => return PlannerOutcome::Success(output),
            Err(err) => {
                tracing::warn!(
                    attempt,
                    max_attempts = PLANNER_ATTEMPTS,
                    err = %err,
                    "planner: attempt failed",
                );
                last = Some(err);
            }
        }
    }
    PlannerOutcome::from(last.expect("loop runs at least once"))
}

/// Assemble the Anthropic Messages request body. Public so tests and future
/// callers can inspect the exact request shape.
pub fn build_request_body(input: &PlannerInput) -> Value {
    json!({
        "model": PLANNER_MODEL,
        "max_tokens": PLANNER_MAX_TOKENS,
        // Bound the reasoning/token spend (design "bound … effort"). Effort
        // lives inside `output_config`, not at the top level.
        "output_config": { "effort": PLANNER_EFFORT },
        "system": SYSTEM_PROMPT,
        // A single forced tool call IS the structured-output mechanism: the
        // model must call `emit_task_graph`, whose `input` is a PlannerOutput.
        "tools": [{
            "name": TOOL_NAME,
            "description": TOOL_DESCRIPTION,
            "input_schema": planner_output_schema(),
        }],
        "tool_choice": { "type": "tool", "name": TOOL_NAME },
        "messages": [{ "role": "user", "content": build_user_prompt(input) }],
    })
}

/// Build the user message: project/product context, the task cap, the
/// existing-task dedup hint, and the full design doc to read.
pub fn build_user_prompt(input: &PlannerInput) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Project: {} (slug: {})\n",
        input.project.name, input.project.slug
    ));
    if !input.project.description.trim().is_empty() {
        out.push_str(&format!("Project description: {}\n", input.project.description));
    }
    if !input.project.goal.trim().is_empty() {
        out.push_str(&format!("Project goal: {}\n", input.project.goal));
    }
    out.push_str(&format!(
        "Product: {} (slug: {})\n\n",
        input.product.name, input.product.slug
    ));

    out.push_str(&format!(
        "Task cap: do NOT propose more than {} task(s). If the doc genuinely \
         describes more, propose the most important up to the cap and say so \
         in `notes`.\n\n",
        input.max_tasks
    ));

    out.push_str(
        "Existing task names already in this project (do NOT propose a task \
         that duplicates one of these; skip any breakdown item whose work \
         they already cover):\n",
    );
    if input.existing_tasks.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for task in &input.existing_tasks {
            out.push_str(&format!("- {}\n", task.name));
        }
        out.push('\n');
    }

    out.push_str(
        "Below is the full merged design document. Read its implementation \
         breakdown and call the `emit_task_graph` tool with the proposed \
         task graph.\n\n",
    );
    out.push_str(&format!("--- BEGIN DESIGN DOC ({}) ---\n", input.design_doc_ref.path));
    out.push_str(&input.design_doc);
    if !input.design_doc.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("--- END DESIGN DOC ---\n");
    out
}

/// Shared HTTP client. Mirrors [`crate::live_status::http_client`] — install
/// the rustls ring provider lazily so the first TLS handshake doesn't panic,
/// and apply the planning wall-clock timeout.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(PLANNER_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

/// Structured error from a single Anthropic call. Mapped into the matching
/// [`PlannerOutcome`] by [`plan_with_url`]. Mirrors
/// [`crate::live_status::SummarizerCallError`] so the surface distinguishes
/// "model 429" from "TLS handshake failed" from "schema mismatch".
#[derive(Debug, thiserror::Error)]
enum PlannerCallError {
    #[error("anthropic returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("failed to decode anthropic response: {0}")]
    Decode(String),
    #[error("invalid planner output: {0}")]
    InvalidOutput(String),
}

impl From<PlannerCallError> for PlannerOutcome {
    fn from(err: PlannerCallError) -> Self {
        match err {
            PlannerCallError::Api { status, body } => PlannerOutcome::ApiError {
                status,
                snippet: clip(&body, 200),
            },
            // Both Transport and Decode are "we couldn't get usable bytes
            // back" — bucket them together, matching live_status.
            PlannerCallError::Transport(msg) | PlannerCallError::Decode(msg) => {
                PlannerOutcome::Transport(msg)
            }
            PlannerCallError::InvalidOutput(msg) => PlannerOutcome::InvalidOutput(msg),
        }
    }
}

/// One round trip: POST the request and extract the forced tool call's input
/// as a [`PlannerOutput`].
async fn call_anthropic(
    url: &str,
    api_key: &str,
    body: &Value,
) -> Result<PlannerOutput, PlannerCallError> {
    let client = http_client();
    let resp = client
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|err| PlannerCallError::Transport(err.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(PlannerCallError::Api { status, body });
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|err| PlannerCallError::Decode(err.to_string()))?;
    planner_output_from_response_json(&value)
}

/// Walk an Anthropic Messages response and pull the forced tool call's
/// `input` out as a [`PlannerOutput`]. Pure and testable: takes the parsed
/// response JSON, so unit tests can exercise it with a `json!` literal.
fn planner_output_from_response_json(body: &Value) -> Result<PlannerOutput, PlannerCallError> {
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PlannerCallError::InvalidOutput("response had no content array".to_owned())
        })?;
    let input = content
        .iter()
        .find(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some(TOOL_NAME)
        })
        .and_then(|block| block.get("input"))
        .ok_or_else(|| {
            PlannerCallError::InvalidOutput(format!(
                "model did not call the {TOOL_NAME} tool"
            ))
        })?;
    serde_json::from_value::<PlannerOutput>(input.clone()).map_err(|err| {
        PlannerCallError::InvalidOutput(format!(
            "tool input did not match the PlannerOutput schema: {err}"
        ))
    })
}

/// Clip a string to `max` bytes on a char boundary, appending an ellipsis if
/// truncated. Used to bound the error snippet stored in [`PlannerOutcome`].
fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

/// The Planner's system prompt. Encodes the coordinator policy a human would
/// otherwise apply by hand: the Q4 effort heuristic, the kind conventions,
/// the parallelism-maximising edge guidance, and the `[effort-classification]`
/// emission contract. See design §2 "Encodes coordinator policy".
const SYSTEM_PROMPT: &str = "\
You are the Boss Planner — a mini-coordinator. You read a merged software \
design document and propose the project's implementation task graph: the \
tasks to create, their effort levels and kinds, and the dependency edges \
between them. You are the automated stand-in for a human coordinator who \
would otherwise read the doc by hand and type out `boss task create` / \
`boss task depend add` calls.\n\
\n\
You write no code and create no rows. Your entire job is the prose-to-typed- \
graph transform: read the doc, then make exactly one `emit_task_graph` tool \
call with the proposed graph. Do not call any other tool.\n\
\n\
## What to extract\n\
\n\
Most design docs end with a section enumerating the implementation work — \
headings like \"Proposed implementation task breakdown\", \"Follow-up \
Implementation Chores\", or \"Implementation Plan\", usually a numbered or \
bulleted list where each item is one bite-sized unit of work (roughly one PR \
each). Extract those items as `tasks`.\n\
\n\
- If the doc contains such a breakdown, set `breakdown_found` to true and \
emit one task per enumerated item.\n\
- If the doc is pure design rationale with NO enumerable implementation \
breakdown, set `breakdown_found` to false, return an empty `tasks` array and \
empty `edges`, and explain in `notes`. This is a clean, valid result — not \
an error. Never invent tasks the doc does not describe.\n\
\n\
Do NOT propose:\n\
- The design task itself (it already exists and its PR has already merged).\n\
- Any task whose name duplicates one already in the project (the existing \
names are listed in the user message).\n\
- More than the task cap stated in the user message.\n\
\n\
## handles\n\
\n\
Each task carries a `handle`: a short, stable, kebab-case proposal-local id \
(e.g. `protocol-types`, `engine-rpc-handler`, `cli-surface`). Handles are \
how edges reference tasks, so make them unique and descriptive. They are not \
shown to users; they exist only to wire the graph.\n\
\n\
## kind conventions\n\
\n\
- Default every task to `project_task`. These belong to a project and map to \
roughly one PR each.\n\
- Use `investigation` for a task framed as research, audit, or diagnosis \
(\"investigate …\", \"audit …\", \"diagnose …\", \"root-cause …\").\n\
- Never emit any other kind. In particular never emit `design` (a project \
has exactly one design task and it already exists) or `chore` (chores are \
product-direct, not project-scoped).\n\
\n\
## effort heuristic (apply per task; first matching rule wins)\n\
\n\
Classify each task into exactly one of `trivial | small | medium | large`. \
Never emit `max` — that level is reserved for explicit human override. \
Evaluate top to bottom and take the first rule that matches:\n\
\n\
1. The task is an investigation / design-flavoured unit (kind = \
investigation, or framed as investigate / audit / instrument / diagnose / \
end-to-end / root cause / architect / redesign / migrate / rearchitect) → \
`large`.\n\
2. The task has very long, substantive scope (a paragraph or more) → \
`large`. Long scope is almost always a project in disguise.\n\
3. The task spans multiple subsystems or names multiple module surfaces \
(\"engine + protocol\", \"across cli and app\", or two or more of: engine, \
cli, protocol, app-macos, cube, bossctl) → `medium`.\n\
4. The task is a near-mechanical single-surface edit (rename / apply / \
revert / bump / move / delete / remove / hide / show / pad / align / \
re-export, a one-line tweak, a cursor / badge / tooltip / gap fix) → \
`trivial`.\n\
5. The task is small and self-contained (one to a few files, no \
architectural judgement) → `small`.\n\
6. Anything else → `medium`.\n\
\n\
As calibration: a schema / protocol / contract task that others build on is \
typically `small`; a single-subsystem feature is `small` or `medium`; an \
integration task that wires several pieces together is `medium`; an \
investigation or multi-subsystem rearchitecture is `large`.\n\
\n\
## [effort-classification] audit line\n\
\n\
For every task produce one `[effort-classification]` line in EXACTLY this \
format (backticks around the level and the rule; double-quoted reasons):\n\
\n\
[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"names engine + protocol surfaces\"\n\
\n\
- Put this line at the END of the task's `description`, separated from the \
rest of the description by a blank line.\n\
- ALSO add the identical line to the `effort_audit` array — one entry per \
task, in the same order as `tasks`.\n\
- The `level` in the line MUST equal the task's `effort`.\n\
\n\
## dependency edges — maximise safe parallelism\n\
\n\
Add an edge ONLY for a true prerequisite: B genuinely cannot start until A \
has landed (e.g. \"engine RPC handler\" depends on \"protocol types\"). \
Leave independently-startable tasks unedged so they dispatch in parallel. Do \
NOT chain tasks into a single line just because they are listed in order — \
`ordinal` already carries the soft ordering hint, and over-edging serializes \
work that could run concurrently.\n\
\n\
The common healthy shape is: a shared schema / protocol / contract task as \
the root, then a fan-out of independent consumer tasks that each depend only \
on that root, then an integration / end-to-end task that depends on the \
fan-out. Edges MUST form a DAG — never introduce a cycle.\n\
\n\
Each edge is { \"dependent\": <handle that waits>, \"prerequisite\": <handle \
that must land first> }. Both endpoints must be handles you emitted.\n\
\n\
## ordinal\n\
\n\
`ordinal` is a soft ordering hint (0, 1, 2, …) suggesting reading order. It \
does NOT gate dispatch — edges do.\n\
\n\
## confidence\n\
\n\
- `high`: the doc has a clear, explicit, well-structured breakdown you \
transcribed with little inference.\n\
- `medium`: you inferred some structure or interpreted an unconventional \
layout.\n\
- `low`: the breakdown was ambiguous or buried, or you are unsure the graph \
is right. (Low blocks nothing downstream — tasks are staged for a human to \
review regardless — but it flags the result for scrutiny.)\n\
\n\
## notes\n\
\n\
Put a short free-text rationale in `notes`: which section you read, how you \
chose the edges, and anything a human reviewer should know.\
";

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{
        Confidence, DocRef, ProductContext, ProjectContext, TaskBrief,
    };
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_input() -> PlannerInput {
        PlannerInput::builder()
            .design_doc("# Design\n\n## Proposed implementation task breakdown\n1. Protocol types.\n2. Engine handler. Depends on 1.\n")
            .design_doc_ref(DocRef {
                repo_remote_url: "https://github.com/owner/repo".to_owned(),
                git_ref: "main".to_owned(),
                path: "tools/boss/docs/designs/foo.md".to_owned(),
            })
            .project(ProjectContext {
                id: "proj_1".to_owned(),
                name: "My Project".to_owned(),
                slug: "my-project".to_owned(),
                description: "Do a thing.".to_owned(),
                goal: "Ship the thing.".to_owned(),
            })
            .product(ProductContext {
                id: "prod_1".to_owned(),
                slug: "boss".to_owned(),
                name: "Boss".to_owned(),
                repo_remote_url: "https://github.com/owner/repo".to_owned(),
            })
            .existing_tasks(vec![TaskBrief {
                id: "task_existing".to_owned(),
                name: "Already here".to_owned(),
            }])
            .max_tasks(30)
            .build()
    }

    /// A well-formed `tool_use` response body mirroring what Anthropic
    /// returns for a forced tool call.
    fn tool_use_response() -> Value {
        json!({
            "content": [
                { "type": "text", "text": "" },
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": TOOL_NAME,
                    "input": {
                        "tasks": [{
                            "handle": "protocol-types",
                            "name": "Add protocol types",
                            "description": "Add the contract types.\n\n[effort-classification] level=`small` matched-rule=`rule 5 (self-contained)` reasons=\"protocol types\"",
                            "kind": "project_task",
                            "effort": "small",
                            "ordinal": 0
                        }, {
                            "handle": "engine-handler",
                            "name": "Engine handler",
                            "description": "Wire the handler.\n\n[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"engine + protocol\"",
                            "kind": "project_task",
                            "effort": "medium",
                            "ordinal": 1
                        }],
                        "edges": [
                            { "dependent": "engine-handler", "prerequisite": "protocol-types" }
                        ],
                        "confidence": "high",
                        "breakdown_found": true,
                        "notes": "Clear two-item breakdown.",
                        "effort_audit": [
                            "[effort-classification] level=`small` matched-rule=`rule 5 (self-contained)` reasons=\"protocol types\"",
                            "[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"engine + protocol\""
                        ]
                    }
                }
            ]
        })
    }

    #[test]
    fn build_request_body_forces_the_planner_tool() {
        let body = build_request_body(&sample_input());
        assert_eq!(body["model"], PLANNER_MODEL);
        assert_eq!(body["max_tokens"], PLANNER_MAX_TOKENS);
        assert_eq!(body["output_config"]["effort"], PLANNER_EFFORT);
        // Structured output is enforced via a forced tool call.
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], TOOL_NAME);
        assert_eq!(body["tools"][0]["name"], TOOL_NAME);
        // The forced tool's input_schema is the contract schema.
        assert_eq!(
            body["tools"][0]["input_schema"],
            planner_output_schema(),
        );
        // System prompt + a single user turn.
        assert!(body["system"].as_str().unwrap().contains("Boss Planner"));
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_user_prompt_carries_doc_and_context() {
        let prompt = build_user_prompt(&sample_input());
        assert!(prompt.contains("My Project"));
        assert!(prompt.contains("Boss"));
        // Task cap surfaced to the model.
        assert!(prompt.contains("more than 30"));
        // Existing-task dedup hint.
        assert!(prompt.contains("Already here"));
        // The full doc is included, fenced by the begin/end markers.
        assert!(prompt.contains("Proposed implementation task breakdown"));
        assert!(prompt.contains("--- BEGIN DESIGN DOC (tools/boss/docs/designs/foo.md) ---"));
        assert!(prompt.contains("--- END DESIGN DOC ---"));
    }

    #[test]
    fn build_user_prompt_handles_no_existing_tasks() {
        let mut input = sample_input();
        input.existing_tasks.clear();
        let prompt = build_user_prompt(&input);
        assert!(prompt.contains("(none)"));
    }

    #[test]
    fn system_prompt_encodes_the_required_policy() {
        // Effort heuristic, kind conventions, parallelism guidance, and the
        // audit-line contract must all be present.
        assert!(SYSTEM_PROMPT.contains("[effort-classification]"));
        assert!(SYSTEM_PROMPT.contains("project_task"));
        assert!(SYSTEM_PROMPT.contains("investigation"));
        assert!(SYSTEM_PROMPT.contains("first matching rule wins"));
        assert!(SYSTEM_PROMPT.contains("Never emit `max`"));
        assert!(SYSTEM_PROMPT.contains("maximise safe parallelism"));
        assert!(SYSTEM_PROMPT.contains("breakdown_found"));
        assert!(SYSTEM_PROMPT.contains("DAG"));
    }

    #[test]
    fn parses_a_well_formed_tool_use_response() {
        let out = planner_output_from_response_json(&tool_use_response())
            .expect("valid tool_use response parses");
        assert_eq!(out.tasks.len(), 2);
        assert_eq!(out.tasks[0].handle, "protocol-types");
        assert_eq!(out.tasks[0].effort, boss_protocol::EffortLevel::Small);
        assert_eq!(out.tasks[1].kind, boss_protocol::TaskKind::ProjectTask);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].dependent, "engine-handler");
        assert_eq!(out.edges[0].prerequisite, "protocol-types");
        assert_eq!(out.confidence, Confidence::High);
        assert!(out.breakdown_found);
        assert_eq!(out.effort_audit.len(), 2);
    }

    #[test]
    fn rejects_response_with_no_tool_call() {
        let body = json!({
            "content": [{ "type": "text", "text": "I could not find a breakdown." }]
        });
        let err = planner_output_from_response_json(&body).unwrap_err();
        assert!(matches!(err, PlannerCallError::InvalidOutput(_)), "got {err:?}");
    }

    #[test]
    fn rejects_tool_input_that_violates_the_schema() {
        // Missing the required `confidence` field.
        let body = json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "breakdown_found": false,
                    "notes": "",
                    "effort_audit": []
                }
            }]
        });
        let err = planner_output_from_response_json(&body).unwrap_err();
        assert!(matches!(err, PlannerCallError::InvalidOutput(_)), "got {err:?}");
    }

    #[test]
    fn no_breakdown_response_is_a_valid_empty_plan() {
        let body = json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": false,
                    "notes": "Pure design rationale; no task breakdown.",
                    "effort_audit": []
                }
            }]
        });
        let out = planner_output_from_response_json(&body).expect("empty plan is valid");
        assert!(out.tasks.is_empty());
        assert!(!out.breakdown_found);
    }

    #[tokio::test]
    async fn plan_returns_no_api_key_when_key_missing() {
        let outcome = Planner::plan(None, &sample_input()).await;
        assert!(matches!(outcome, PlannerOutcome::NoApiKey));
        assert_eq!(outcome.tag(), "no_api_key");
    }

    #[tokio::test]
    async fn end_to_end_success_against_mock_anthropic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response()))
            .mount(&server)
            .await;

        let outcome = plan_with_url(
            &format!("{}/v1/messages", server.uri()),
            "test-key",
            &sample_input(),
        )
        .await;

        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 2);
                assert_eq!(out.edges.len(), 1);
                assert_eq!(out.confidence, Confidence::High);
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retries_once_then_succeeds() {
        let server = MockServer::start().await;
        // First call: a transient 503 (consumed once). Second call: success.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response()))
            .mount(&server)
            .await;

        let outcome = plan_with_url(
            &format!("{}/v1/messages", server.uri()),
            "test-key",
            &sample_input(),
        )
        .await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "expected success after one retry, got {outcome:?}",
        );
    }

    #[tokio::test]
    async fn api_error_after_exhausting_retries() {
        let server = MockServer::start().await;
        // Always 401 — not retryable in practice, but we still attempt twice
        // and then fail safe with the typed ApiError outcome.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let outcome = plan_with_url(
            &format!("{}/v1/messages", server.uri()),
            "test-key",
            &sample_input(),
        )
        .await;
        match outcome {
            PlannerOutcome::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got {other:?}"),
        }
        assert_eq!(outcome.tag(), "api_error");
    }

    #[test]
    fn outcome_tags_are_stable() {
        assert_eq!(PlannerOutcome::NoApiKey.tag(), "no_api_key");
        assert_eq!(
            PlannerOutcome::ApiError { status: 429, snippet: "x".into() }.tag(),
            "api_error",
        );
        assert_eq!(
            PlannerOutcome::Transport("boom".into()).tag(),
            "transport_error",
        );
        assert_eq!(
            PlannerOutcome::InvalidOutput("nope".into()).tag(),
            "invalid_output",
        );
    }
}
