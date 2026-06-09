//! Planner contract types shared between `boss-engine` callers and tests.
//!
//! The Planner is a reusable LLM mini-coordinator:
//! `Planner::plan(PlannerInput) -> Result<PlannerOutput>`. These types define
//! the typed contract so every caller speaks the same shape.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! for the full design.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{EffortLevel, TaskKind};

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// Provenance record for the design doc fetched by the Planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocRef {
    /// Canonical remote URL of the repository the doc lives in.
    pub repo_remote_url: String,
    /// The branch name or commit SHA at which the doc was fetched.
    pub git_ref: String,
    /// Repo-relative path to the design doc, e.g. `tools/boss/docs/designs/foo.md`.
    pub path: String,
}

/// Slim project view supplied to the Planner as context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
}

/// Slim product view supplied to the Planner as context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductContext {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub repo_remote_url: String,
}

/// Minimal task record passed to the Planner so it can avoid proposing
/// tasks whose names duplicate ones already in the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBrief {
    pub id: String,
    pub name: String,
}

/// All inputs the Planner needs to produce a task-graph proposal.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct PlannerInput {
    /// Full text of the merged design doc fetched live from GitHub.
    pub design_doc: String,
    /// Provenance record so the audit trail can reproduce the fetch.
    pub design_doc_ref: DocRef,
    /// Project the tasks will be created in.
    pub project: ProjectContext,
    /// Product the project belongs to.
    pub product: ProductContext,
    /// Tasks already in the project — a dedup hint for the Planner.
    pub existing_tasks: Vec<TaskBrief>,
    /// Hard cap surfaced to the model; proposals exceeding it are rejected.
    pub max_tasks: usize,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// The Planner's confidence in the task-graph it extracted.
///
/// `Low` does not block materialization — tasks are always staged with
/// `autostart = false` and require operator release regardless — but it
/// escalates the attention-item prominence so the operator knows to
/// scrutinize the plan before releasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single task proposed by the Planner.
///
/// `handle` is a proposal-local identifier (e.g. `"schema-migration"`) used
/// to reference this task in [`ProposedEdge`] dependency declarations.  The
/// Materializer resolves handles to real task ids at apply time.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct ProposedTask {
    /// Proposal-local identifier; referenced by [`ProposedEdge`].
    pub handle: String,
    pub name: String,
    pub description: String,
    /// `project_task` (default) or `investigation`.  The Planner never
    /// emits `design`, `chore`, `revision`, or `task`.
    pub kind: TaskKind,
    /// Effort estimate; the Planner never emits `max` (human-only).
    pub effort: EffortLevel,
    /// Soft ordering hint — not a hard dependency gate (edges are).
    pub ordinal: i64,
}

/// A directed dependency edge between two proposed tasks, expressed by handle.
///
/// Semantics: `prerequisite` must land before `dependent` can start.
/// Mirrors the `blocks` relation in `add_dependency`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedEdge {
    /// Handle of the task that is gated (must wait for the prerequisite).
    pub dependent: String,
    /// Handle of the task that gates it.
    pub prerequisite: String,
}

/// The Planner's structured output — a validated, typed task-graph proposal.
///
/// This is the shape the engine deserialises directly from the Anthropic
/// structured-output call.  A JSON Schema for this type is exported by
/// [`planner_output_schema`] for use as the forced tool-call `input_schema`.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct PlannerOutput {
    pub tasks: Vec<ProposedTask>,
    /// Dependency edges between tasks, referenced by handle.
    pub edges: Vec<ProposedEdge>,
    pub confidence: Confidence,
    /// `false` when the design doc contained no task-breakdown section at
    /// all — a clean no-op signal, distinct from "found a breakdown but it
    /// was empty".
    pub breakdown_found: bool,
    /// Free-text rationale from the Planner, persisted in `planner_runs` for
    /// the operator to inspect after the fact.
    pub notes: String,
    /// One `[effort-classification] …` line per proposed task, in the same
    /// format the coordinator and engine emit today.
    pub effort_audit: Vec<String>,
}

// ---------------------------------------------------------------------------
// Apply result
// ---------------------------------------------------------------------------

/// Result returned by `Materializer::apply` after a successful (or partially
/// deduped) application of a [`PlannerOutput`] proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// IDs of tasks created in this run (already existed rows are in `skipped`).
    pub created: Vec<String>,
    /// Names of tasks that were skipped because a non-deleted task with that
    /// name already existed in the project.
    pub skipped: Vec<String>,
    /// Number of dependency edges inserted.
    pub edges_created: usize,
}

// ---------------------------------------------------------------------------
// JSON Schema for structured-output enforcement
// ---------------------------------------------------------------------------

/// Returns the JSON Schema used as the `input_schema` of the Anthropic forced
/// tool-call that constrains the Planner's response to [`PlannerOutput`].
///
/// The schema is intentionally conservative: it marks every field `required`
/// and enumerates the legal values for all enum fields, so a deserialization
/// failure means the model returned something outside the contract rather than
/// a missing optional.
pub fn planner_output_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "tasks",
            "edges",
            "confidence",
            "breakdown_found",
            "notes",
            "effort_audit"
        ],
        "additionalProperties": false,
        "properties": {
            "tasks": {
                "type": "array",
                "description": "Proposed implementation tasks extracted from the design doc.",
                "items": {
                    "type": "object",
                    "required": ["handle", "name", "description", "kind", "effort", "ordinal"],
                    "additionalProperties": false,
                    "properties": {
                        "handle": {
                            "type": "string",
                            "description": "Proposal-local identifier for this task, referenced in edges."
                        },
                        "name": {
                            "type": "string",
                            "description": "Short task name as it will appear in Boss."
                        },
                        "description": {
                            "type": "string",
                            "description": "Full task description, including the [effort-classification] audit line."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["project_task", "investigation"],
                            "description": "Task kind. Use project_task by default; investigation for research/audit/diagnose tasks."
                        },
                        "effort": {
                            "type": "string",
                            "enum": ["trivial", "small", "medium", "large"],
                            "description": "Effort estimate. Never use max (human-only)."
                        },
                        "ordinal": {
                            "type": "integer",
                            "description": "Soft ordering hint; not a hard dependency gate."
                        }
                    }
                }
            },
            "edges": {
                "type": "array",
                "description": "Dependency edges between proposed tasks, by handle.",
                "items": {
                    "type": "object",
                    "required": ["dependent", "prerequisite"],
                    "additionalProperties": false,
                    "properties": {
                        "dependent": {
                            "type": "string",
                            "description": "Handle of the task that is gated (must wait)."
                        },
                        "prerequisite": {
                            "type": "string",
                            "description": "Handle of the task that gates it (must land first)."
                        }
                    }
                }
            },
            "confidence": {
                "type": "string",
                "enum": ["high", "medium", "low"],
                "description": "Planner's confidence in the quality of this proposal."
            },
            "breakdown_found": {
                "type": "boolean",
                "description": "true if the doc contained a task-breakdown section; false for a clean no-op."
            },
            "notes": {
                "type": "string",
                "description": "Free-text rationale persisted in planner_runs for operator review."
            },
            "effort_audit": {
                "type": "array",
                "description": "One [effort-classification] line per proposed task.",
                "items": {
                    "type": "string"
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_output_schema_is_valid_json() {
        let schema = planner_output_schema();
        // Must serialise without panic and contain required keys.
        let s = serde_json::to_string(&schema).expect("schema serialises");
        assert!(s.contains("\"tasks\""));
        assert!(s.contains("\"edges\""));
        assert!(s.contains("\"confidence\""));
        assert!(s.contains("\"breakdown_found\""));
    }

    #[test]
    fn planner_output_round_trips() {
        let output = PlannerOutput {
            tasks: vec![ProposedTask {
                handle: "schema".into(),
                name: "Add schema".into(),
                description: "Add the schema types.".into(),
                kind: TaskKind::ProjectTask,
                effort: EffortLevel::Small,
                ordinal: 1,
            }],
            edges: vec![],
            confidence: Confidence::High,
            breakdown_found: true,
            notes: "Clear breakdown found.".into(),
            effort_audit: vec![
                "[effort-classification] level=`small` matched-rule=`rule 1` reasons=\"protocol types\"".into(),
            ],
        };

        let json = serde_json::to_string(&output).expect("serialises");
        let back: PlannerOutput = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].handle, "schema");
        assert_eq!(back.confidence, Confidence::High);
        assert!(back.breakdown_found);
    }

    #[test]
    fn apply_result_round_trips() {
        let result = ApplyResult {
            created: vec!["task_abc".into(), "task_def".into()],
            skipped: vec!["Existing task".into()],
            edges_created: 1,
        };

        let json = serde_json::to_string(&result).expect("serialises");
        let back: ApplyResult = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.created.len(), 2);
        assert_eq!(back.skipped.len(), 1);
        assert_eq!(back.edges_created, 1);
    }

    #[test]
    fn confidence_display() {
        assert_eq!(Confidence::High.as_str(), "high");
        assert_eq!(Confidence::Medium.as_str(), "medium");
        assert_eq!(Confidence::Low.as_str(), "low");
        assert_eq!(Confidence::High.to_string(), "high");
    }
}
