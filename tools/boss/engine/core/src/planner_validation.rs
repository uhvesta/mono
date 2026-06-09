//! Validation layer for [`PlannerOutput`] proposals.
//!
//! Sits between the Planner (LLM inference) and the Materializer (DB writes).
//! Every check here is **no-op-safe**: nothing is read from or written to the
//! database. On any rejection the caller maps the variant to its
//! `PLANNER_OUTCOME_*` constant and raises an attention item; no tasks are
//! created.
//!
//! See design §"Validation of the structured proposal":
//! `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`

use std::collections::{HashMap, HashSet};

use boss_protocol::{Confidence, PlannerOutput, ProposedEdge};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The typed result of validating a [`PlannerOutput`] proposal.
///
/// All rejection variants are no-op-safe: nothing has been written before or
/// during validation. The Populator maps each variant to its
/// `PLANNER_OUTCOME_*` constant and raises an attention item.
///
/// Only `Valid` should proceed to the Materializer.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidationResult {
    /// `breakdown_found == false` — the design doc had no task-breakdown
    /// section. Clean no-op; maps to `PLANNER_OUTCOME_NO_BREAKDOWN`.
    NoBreakdown,

    /// `breakdown_found == true` but `tasks` is empty — the planner found a
    /// section but extracted nothing meaningful from it. No-op + attention item.
    EmptyBreakdown,

    /// `tasks.len() > max_tasks`. Silent truncation is forbidden per the
    /// design; the whole proposal is rejected.
    /// Maps to `PLANNER_OUTCOME_REJECTED_TOO_MANY`.
    RejectedTooMany {
        count: usize,
        max: usize,
    },

    /// A handle appears more than once in the `tasks` list.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE` (re-uses the "bad graph" bucket).
    RejectedDuplicateHandle {
        handle: String,
    },

    /// An edge references a handle not present in the `tasks` list.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE` (re-uses the "bad graph" bucket).
    RejectedUnknownHandle {
        handle: String,
    },

    /// The proposed edge set forms a dependency cycle.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE`.
    /// `cycle` is a representative cycle path expressed as handle names;
    /// the last element is the back-edge target that also appears earlier
    /// in the list, making the loop explicit.
    RejectedCycle {
        cycle: Vec<String>,
    },

    /// All checks passed; the proposal is ready for the Materializer.
    Valid {
        /// `true` when the planner returned `Confidence::Low`. The proposal
        /// is still materialised (staged), but the attention item should be
        /// escalated in prominence so the operator scrutinises the plan
        /// before releasing.
        low_confidence: bool,
    },
}

// ---------------------------------------------------------------------------
// Validation entry point
// ---------------------------------------------------------------------------

/// Validate a [`PlannerOutput`] proposal before handing it to the Materializer.
///
/// Checks run in order, short-circuiting on the first failure:
///
/// 1. `breakdown_found == false` → [`ValidationResult::NoBreakdown`]
/// 2. `tasks.is_empty()` with `breakdown_found == true` → [`ValidationResult::EmptyBreakdown`]
/// 3. `tasks.len() > max_tasks` → [`ValidationResult::RejectedTooMany`]
/// 4. Duplicate handle in `tasks` → [`ValidationResult::RejectedDuplicateHandle`]
/// 5. Edge references unknown handle → [`ValidationResult::RejectedUnknownHandle`]
/// 6. Edge set contains a cycle → [`ValidationResult::RejectedCycle`]
/// 7. Otherwise → [`ValidationResult::Valid`] (`low_confidence` set when
///    `confidence == Confidence::Low`)
///
/// This function performs no I/O and has no side effects.
pub fn validate(output: &PlannerOutput, max_tasks: usize) -> ValidationResult {
    // 1. No breakdown section in the doc at all.
    if !output.breakdown_found {
        return ValidationResult::NoBreakdown;
    }

    // 2. Breakdown section present but nothing extracted.
    if output.tasks.is_empty() {
        return ValidationResult::EmptyBreakdown;
    }

    // 3. Proposal exceeds the task cap — reject whole, never truncate.
    if output.tasks.len() > max_tasks {
        return ValidationResult::RejectedTooMany {
            count: output.tasks.len(),
            max: max_tasks,
        };
    }

    // 4. Handle uniqueness — every handle must appear exactly once.
    let mut known: HashSet<&str> = HashSet::with_capacity(output.tasks.len());
    for task in &output.tasks {
        if !known.insert(task.handle.as_str()) {
            return ValidationResult::RejectedDuplicateHandle {
                handle: task.handle.clone(),
            };
        }
    }

    // 5. Handle integrity — every edge endpoint must name a known handle.
    for edge in &output.edges {
        if !known.contains(edge.dependent.as_str()) {
            return ValidationResult::RejectedUnknownHandle {
                handle: edge.dependent.clone(),
            };
        }
        if !known.contains(edge.prerequisite.as_str()) {
            return ValidationResult::RejectedUnknownHandle {
                handle: edge.prerequisite.clone(),
            };
        }
    }

    // 6. Acyclicity — the edge set must form a DAG.
    if let Some(cycle) = detect_cycle(&known, &output.edges) {
        return ValidationResult::RejectedCycle { cycle };
    }

    // 7. All checks passed.
    ValidationResult::Valid {
        low_confidence: output.confidence == Confidence::Low,
    }
}

// ---------------------------------------------------------------------------
// Cycle detection (in-memory, handle graph)
// ---------------------------------------------------------------------------

/// Returns `Some(cycle_path)` if the directed graph defined by `edges` over
/// `handles` contains at least one cycle; otherwise returns `None`.
///
/// Edge direction: `dependent → prerequisite` (dependent depends on
/// prerequisite). A cycle exists when following this direction from some
/// handle eventually leads back to itself.
///
/// The returned `cycle_path` is a sequence of handle names where each entry
/// is a prerequisite of the next; the last entry matches an earlier entry,
/// making the cycle explicit (e.g. `["A", "B", "C", "A"]`).
///
/// Uses iterative DFS with three-colour marking (white → gray → black) to
/// avoid call-stack depth limits on large proposals.
fn detect_cycle<'a>(handles: &HashSet<&'a str>, edges: &'a [ProposedEdge]) -> Option<Vec<String>> {
    // Adjacency list: dependent → list of prerequisites it depends on.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::with_capacity(handles.len());
    for &h in handles {
        adj.entry(h).or_default();
    }
    for edge in edges {
        adj.entry(edge.dependent.as_str())
            .or_default()
            .push(edge.prerequisite.as_str());
    }

    // Three-colour DFS state.
    const WHITE: u8 = 0; // not yet visited
    const GRAY: u8 = 1;  // on the current DFS path
    const BLACK: u8 = 2; // fully explored, no cycle through here

    let mut color: HashMap<&str, u8> = handles.iter().map(|&h| (h, WHITE)).collect();
    // `path` mirrors the DFS stack: the sequence of handles on the current
    // path from the DFS root to the node being explored.
    let mut path: Vec<String> = Vec::new();

    for &start in handles {
        if color[start] != WHITE {
            continue;
        }

        // Push the start node and begin iterative DFS.
        color.insert(start, GRAY);
        path.push(start.to_owned());
        // Stack frames: (handle, next-neighbor-index-to-examine).
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];

        while let Some(frame) = stack.last_mut() {
            let node = frame.0;
            let prereqs: &[&str] = adj.get(node).map(|v| v.as_slice()).unwrap_or(&[]);

            if frame.1 < prereqs.len() {
                let next = prereqs[frame.1];
                frame.1 += 1;

                match color.get(next).copied().unwrap_or(WHITE) {
                    GRAY => {
                        // Back edge — cycle detected.
                        // Find where `next` appears in the current path.
                        let pos = path.iter().position(|s| s.as_str() == next).unwrap_or(0);
                        let mut cycle = path[pos..].to_vec();
                        cycle.push(next.to_owned()); // close the loop
                        return Some(cycle);
                    }
                    WHITE => {
                        color.insert(next, GRAY);
                        path.push(next.to_owned());
                        stack.push((next, 0));
                    }
                    _ => {} // BLACK: already fully explored, no cycle through here
                }
            } else {
                // All neighbors explored for `node`; mark done and retreat.
                color.insert(node, BLACK);
                path.pop();
                stack.pop();
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use boss_protocol::{EffortLevel, ProposedTask, TaskKind};

    use super::*;

    // ---- helpers -----------------------------------------------------------

    fn task(handle: &str) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: format!("Task {handle}"),
            description: "desc".to_owned(),
            kind: TaskKind::ProjectTask,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn edge(dep: &str, pre: &str) -> ProposedEdge {
        ProposedEdge {
            dependent: dep.to_owned(),
            prerequisite: pre.to_owned(),
        }
    }

    fn output_with(
        tasks: Vec<ProposedTask>,
        edges: Vec<ProposedEdge>,
        confidence: Confidence,
        breakdown_found: bool,
    ) -> PlannerOutput {
        PlannerOutput {
            tasks,
            edges,
            confidence,
            breakdown_found,
            notes: String::new(),
            effort_audit: vec![],
        }
    }

    // ---- NoBreakdown -------------------------------------------------------

    #[test]
    fn no_breakdown_when_flag_is_false() {
        let out = output_with(vec![task("t1")], vec![], Confidence::High, false);
        assert_eq!(validate(&out, 30), ValidationResult::NoBreakdown);
    }

    // ---- EmptyBreakdown ----------------------------------------------------

    #[test]
    fn empty_breakdown_when_tasks_empty_and_breakdown_found() {
        let out = output_with(vec![], vec![], Confidence::High, true);
        assert_eq!(validate(&out, 30), ValidationResult::EmptyBreakdown);
    }

    // ---- RejectedTooMany ---------------------------------------------------

    #[test]
    fn rejected_too_many_when_over_cap() {
        let tasks: Vec<_> = (0..5).map(|i| task(&format!("t{i}"))).collect();
        let out = output_with(tasks, vec![], Confidence::High, true);
        assert_eq!(
            validate(&out, 4),
            ValidationResult::RejectedTooMany { count: 5, max: 4 }
        );
    }

    #[test]
    fn not_rejected_when_exactly_at_cap() {
        let tasks: Vec<_> = (0..5).map(|i| task(&format!("t{i}"))).collect();
        let out = output_with(tasks, vec![], Confidence::High, true);
        // Exactly at cap (5 tasks, max_tasks = 5) is valid.
        assert!(matches!(validate(&out, 5), ValidationResult::Valid { .. }));
    }

    // ---- RejectedDuplicateHandle -------------------------------------------

    #[test]
    fn rejected_on_duplicate_handle() {
        let out = output_with(
            vec![task("alpha"), task("alpha")],
            vec![],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedDuplicateHandle {
                handle: "alpha".to_owned()
            }
        );
    }

    // ---- RejectedUnknownHandle ---------------------------------------------

    #[test]
    fn rejected_when_dependent_handle_unknown() {
        let out = output_with(
            vec![task("schema"), task("engine")],
            vec![edge("ghost", "schema")],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedUnknownHandle {
                handle: "ghost".to_owned()
            }
        );
    }

    #[test]
    fn rejected_when_prerequisite_handle_unknown() {
        let out = output_with(
            vec![task("schema"), task("engine")],
            vec![edge("engine", "ghost")],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedUnknownHandle {
                handle: "ghost".to_owned()
            }
        );
    }

    // ---- RejectedCycle -----------------------------------------------------

    #[test]
    fn rejected_cycle_simple_two_node() {
        // A depends on B, B depends on A.
        let out = output_with(
            vec![task("a"), task("b")],
            vec![edge("a", "b"), edge("b", "a")],
            Confidence::High,
            true,
        );
        assert!(matches!(
            validate(&out, 30),
            ValidationResult::RejectedCycle { .. }
        ));
    }

    #[test]
    fn rejected_cycle_three_node_ring() {
        // A → B → C → A (each depends on the next).
        let out = output_with(
            vec![task("a"), task("b"), task("c")],
            vec![edge("a", "b"), edge("b", "c"), edge("c", "a")],
            Confidence::High,
            true,
        );
        let result = validate(&out, 30);
        match result {
            ValidationResult::RejectedCycle { cycle } => {
                // Cycle must be non-empty and the last element must repeat an
                // earlier one (closing the loop).
                assert!(cycle.len() >= 2);
                let last = cycle.last().unwrap();
                assert!(cycle[..cycle.len() - 1].contains(last));
            }
            other => panic!("expected RejectedCycle, got {other:?}"),
        }
    }

    #[test]
    fn rejected_cycle_self_loop() {
        // A depends on itself.
        let out = output_with(
            vec![task("a")],
            vec![edge("a", "a")],
            Confidence::High,
            true,
        );
        assert!(matches!(
            validate(&out, 30),
            ValidationResult::RejectedCycle { .. }
        ));
    }

    // ---- Valid -------------------------------------------------------------

    #[test]
    fn valid_dag_single_task_no_edges() {
        let out = output_with(vec![task("schema")], vec![], Confidence::High, true);
        assert_eq!(
            validate(&out, 30),
            ValidationResult::Valid { low_confidence: false }
        );
    }

    #[test]
    fn valid_dag_linear_chain() {
        // schema → engine → integration (schema is prerequisite of engine,
        // engine is prerequisite of integration).
        let out = output_with(
            vec![task("schema"), task("engine"), task("integration")],
            vec![edge("engine", "schema"), edge("integration", "engine")],
            Confidence::Medium,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::Valid { low_confidence: false }
        );
    }

    #[test]
    fn valid_dag_fan_out() {
        // schema is prerequisite for both engine and cli; engine and cli
        // are independent (no edge between them).
        let out = output_with(
            vec![task("schema"), task("engine"), task("cli")],
            vec![edge("engine", "schema"), edge("cli", "schema")],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::Valid { low_confidence: false }
        );
    }

    // ---- Low confidence ----------------------------------------------------

    #[test]
    fn valid_with_low_confidence_flag_set() {
        let out = output_with(vec![task("t1")], vec![], Confidence::Low, true);
        assert_eq!(
            validate(&out, 30),
            ValidationResult::Valid { low_confidence: true }
        );
    }

    #[test]
    fn valid_medium_confidence_not_flagged() {
        let out = output_with(vec![task("t1")], vec![], Confidence::Medium, true);
        assert_eq!(
            validate(&out, 30),
            ValidationResult::Valid { low_confidence: false }
        );
    }

    // ---- Ordering: breakdown_found takes priority over empty tasks ---------

    #[test]
    fn no_breakdown_takes_priority_over_empty_tasks() {
        // Even if tasks is empty, breakdown_found = false wins.
        let out = output_with(vec![], vec![], Confidence::High, false);
        assert_eq!(validate(&out, 30), ValidationResult::NoBreakdown);
    }

    // ---- Ordering: cap check before handle checks --------------------------

    #[test]
    fn too_many_takes_priority_over_duplicate_handles() {
        // 3 tasks (max_tasks = 2), two of which share a handle.
        let tasks = vec![task("a"), task("b"), task("a")];
        let out = output_with(tasks, vec![], Confidence::High, true);
        // Should hit RejectedTooMany before RejectedDuplicateHandle.
        assert_eq!(
            validate(&out, 2),
            ValidationResult::RejectedTooMany { count: 3, max: 2 }
        );
    }
}
