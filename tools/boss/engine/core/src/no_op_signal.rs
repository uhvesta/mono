//! Sanctioned "nothing to do / already-done" terminal signal for
//! primary-implementation workers.
//!
//! A `chore_implementation` / `task_implementation` worker sometimes
//! discovers, after investigating, that the work is *already done* — the
//! change is already present on `main`, so `jj diff -r @` is empty and
//! there is genuinely nothing to commit, push, or open a PR for. That is
//! a legitimate, successful no-op, NOT a failure.
//!
//! Before this signal existed the worker had no sanctioned way to say so:
//! the prompt told it to "stop and explain", but the engine's Stop-boundary
//! handler then read the empty branch as "stopped without producing a PR"
//! and nudged it to `gh pr create` — the two instructions were in direct
//! conflict, and the worker churned against the nudge until the circuit
//! breaker parked it for a human (incident `exec_18b9771d36ed67b8_b02`,
//! T1868 — a comment-sweep chore two sibling PRs had already cleaned).
//!
//! This module defines the marker the worker emits to break that tie and
//! the parser the engine uses to recognise it. The engine accepts the
//! marker as a terminal completion ONLY in combination with a structurally
//! empty contribution (no PR on the branch, none bound to the chore), so a
//! worker that merely *gave up without trying* — and never emitted the
//! marker — still gets the legitimate "produce a PR" nudge. See
//! [`crate::completion::WorkerCompletionHandler`]'s no-op gate.
//!
//! The matching discipline mirrors [`crate::automation_triage`]'s decision
//! markers: a line whose trimmed, decoration-stripped content equals the
//! marker exactly. Prose that merely *mentions* the protocol (e.g. an
//! explanation of when to use it) does not trip it.

/// The sanctioned marker a primary-implementation worker emits, on its own
/// line, to signal that the assigned work is already done and there is
/// nothing to commit / push / open a PR for. The engine treats this as a
/// successful no-op terminal (the task is closed as done without a PR)
/// rather than nudging the worker to produce a PR.
pub const NO_CHANGES_NEEDED_MARKER: &str = "NO_CHANGES_NEEDED";

/// True when `text` (the worker's final assistant prose) contains the
/// [`NO_CHANGES_NEEDED_MARKER`] on a line of its own.
///
/// A line matches when, after trimming surrounding whitespace and stripping
/// common Markdown decoration (backticks, asterisks, leading list markers),
/// its content equals the marker exactly (case-sensitive). Requiring an
/// own-line exact match — not a substring scan — means prose that quotes or
/// explains the protocol ("emit `NO_CHANGES_NEEDED` to signal …") does not
/// falsely trip the gate.
pub fn transcript_signals_no_op(text: &str) -> bool {
    text.lines().any(line_is_no_op_marker)
}

/// Whether a single line, once decoration is stripped, is exactly the marker.
fn line_is_no_op_marker(line: &str) -> bool {
    strip_decoration(line) == NO_CHANGES_NEEDED_MARKER
}

/// Strip surrounding whitespace and the Markdown decoration a worker is
/// likely to wrap the marker in: a leading list bullet (`- `, `* `, `+ `),
/// then surrounding backticks / asterisks / spaces. The marker itself is
/// `[A-Z_]`, so trimming these characters can never eat into it.
fn strip_decoration(line: &str) -> &str {
    let trimmed = line.trim();
    // Drop a single leading list-bullet marker if present.
    let without_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .unwrap_or(trimmed);
    without_bullet.trim_matches(|c: char| c == '`' || c == '*' || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_marker_on_its_own_line_matches() {
        assert!(transcript_signals_no_op("NO_CHANGES_NEEDED"));
    }

    #[test]
    fn marker_among_other_lines_matches() {
        let text =
            "## Summary\nThe three breadcrumb patterns were already cleaned by #1559 and #1561.\n\nNO_CHANGES_NEEDED\n";
        assert!(transcript_signals_no_op(text));
    }

    #[test]
    fn backtick_wrapped_marker_matches() {
        assert!(transcript_signals_no_op("`NO_CHANGES_NEEDED`"));
    }

    #[test]
    fn bullet_and_backtick_wrapped_marker_matches() {
        assert!(transcript_signals_no_op("- `NO_CHANGES_NEEDED`"));
    }

    #[test]
    fn bold_wrapped_marker_matches() {
        assert!(transcript_signals_no_op("**NO_CHANGES_NEEDED**"));
    }

    #[test]
    fn indented_marker_matches() {
        assert!(transcript_signals_no_op("    NO_CHANGES_NEEDED   "));
    }

    #[test]
    fn marker_mentioned_in_prose_does_not_match() {
        // The worker explaining the protocol must NOT trip the gate — only
        // an own-line emission counts.
        let text = "I considered emitting NO_CHANGES_NEEDED but I still have edits to make.";
        assert!(!transcript_signals_no_op(text));
    }

    #[test]
    fn marker_with_trailing_prose_on_same_line_does_not_match() {
        let text = "NO_CHANGES_NEEDED because the work was already done";
        assert!(!transcript_signals_no_op(text));
    }

    #[test]
    fn lowercase_variant_does_not_match() {
        assert!(!transcript_signals_no_op("no_changes_needed"));
    }

    #[test]
    fn empty_text_does_not_match() {
        assert!(!transcript_signals_no_op(""));
    }

    #[test]
    fn unrelated_text_does_not_match() {
        assert!(!transcript_signals_no_op(
            "## Summary\nMade the change and opened a PR.\n"
        ));
    }
}
