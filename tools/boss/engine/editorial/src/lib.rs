//! Pure evaluator for editorial rules applied to worker-authored PR/issue text.
//!
//! `evaluate(body, title, rules)` is the single entry point. It is a pure
//! function — no I/O, no global mutation — so it is trivially testable and
//! safe to call on the engine's async task threads.
//!
//! ## Baked-in defaults
//!
//! Every product gets the baked-in redaction list applied, with or without
//! user-configured rules:
//!
//! - **Rewrite** (strip + collapse whitespace): Boss identifier shapes
//!   `exec_…`, `proj_…`, `task_…`, `chg_…`, `boss/exec_…` branch prefixes,
//!   UUIDs that appear within ~40 chars of "lease" or "cube".
//! - **Block** (deny the `gh` invocation with actionable feedback): free-text
//!   phrases that expose Boss internals — "Boss worker", "the engine", etc.
//!
//! ## Markdown awareness (R2)
//!
//! The scanner splits `body` into segments before applying patterns:
//!
//! - **Fenced code blocks** (``` or ~~~): completely skipped for all patterns.
//! - **Inline code spans** (backtick-quoted): baked-in Rewrite patterns are
//!   only applied when the entire span content (stripping the backticks)
//!   matches the pattern — i.e., `exec_abc123_xy` triggers but a longer
//!   sentence fragment inside backticks does not.
//! - **Plain text**: all patterns applied normally.
//!
//! ## Template conformance (R4)
//!
//! When `template_policy == Enforce` and a `template_body` is supplied,
//! `evaluate` extracts the H2 / H3 headings from the template, checks that
//! each one appears in the PR body, and emits a `Block` finding per missing
//! heading.
//!
//! ## Performance
//!
//! Baked-in regexes are compiled once into a `LazyLock` and reused across
//! calls. User-supplied regexes should be pre-compiled via `CompiledRules`
//! before the hot path; `evaluate` accepts a `&CompiledRules` so callers can
//! amortise compilation cost across multiple calls.

use std::sync::LazyLock;

use regex::Regex;

use boss_protocol::{EditorialRules, RedactionKind, TemplatePolicy};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The outcome of `evaluate`. Callers turn this into a PreToolUse decision.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorialDecision {
    /// No violations. Allow the `gh` invocation unchanged.
    Allow,
    /// All violations are auto-rewritable. The `body` and optional `title`
    /// fields hold the sanitised text. `findings` lists what was changed so
    /// callers can emit a `decisionReason`.
    Rewrite {
        body: String,
        title: Option<String>,
        findings: Vec<Finding>,
    },
    /// At least one violation cannot be auto-corrected (a `Block`-kind
    /// finding or a missing template section). The `gh` call should be
    /// denied and the worker told to fix the body.
    Block { findings: Vec<Finding> },
}

/// One violation surfaced during evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    /// Human-readable description suitable for inclusion in `decisionReason`.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// Pattern matched and the body was (or should be) rewritten in-place.
    Redact,
    /// Phrase matched; the worker must rephrase rather than auto-strip.
    Block,
    /// Template heading is absent from the body.
    Template,
}

// ---------------------------------------------------------------------------
// CompiledRules — pre-compiled user regex cache
// ---------------------------------------------------------------------------

/// User-supplied redaction rules with their regexes pre-compiled.
///
/// Compile once with `CompiledRules::compile` at engine startup or when the
/// product's `EditorialRules` change; pass `&CompiledRules` into `evaluate`
/// on every hot call.
pub struct CompiledRules {
    pub source: EditorialRules,
    compiled: Vec<(Regex, String, RedactionKind)>,
}

impl CompiledRules {
    /// Compile the user-supplied regex patterns in `rules`. Returns an error
    /// if any pattern is syntactically invalid.
    pub fn compile(rules: EditorialRules) -> Result<Self, regex::Error> {
        let mut compiled = Vec::with_capacity(rules.redactions.len());
        for r in &rules.redactions {
            let re = Regex::new(&r.pattern)?;
            compiled.push((re, r.replacement.clone(), r.kind.clone()));
        }
        Ok(Self { source: rules, compiled })
    }
}

// ---------------------------------------------------------------------------
// Baked-in patterns
// ---------------------------------------------------------------------------

macro_rules! lazy_re {
    ($pat:expr) => {{
        static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("baked-in regex"));
        &RE
    }};
}

fn baked_in_rewrite_patterns() -> &'static [(&'static str, &'static LazyLock<Regex>)] {
    static PATTERNS: LazyLock<Vec<(&'static str, &'static LazyLock<Regex>)>> =
        LazyLock::new(|| {
            vec![
                // Execution ids
                (
                    "exec_… identifier",
                    lazy_re!(r"exec_[0-9a-f]{16}_[A-Za-z0-9]+"),
                ),
                // Project ids
                (
                    "proj_… identifier",
                    lazy_re!(r"proj_[0-9a-f]{16}_[A-Za-z0-9]+"),
                ),
                // Task ids
                (
                    "task_… identifier",
                    lazy_re!(r"task_[0-9a-f]{16}_[A-Za-z0-9]+"),
                ),
                // Chore / change ids
                ("chg_… identifier", lazy_re!(r"chg_[0-9a-f]{32}")),
                // boss/exec_… branch-name substrings
                (
                    "boss/exec_… branch name",
                    lazy_re!(r"boss/exec_[A-Za-z0-9_/-]+"),
                ),
            ]
        });
    &PATTERNS
}

fn baked_in_block_patterns() -> &'static [(&'static str, &'static LazyLock<Regex>)] {
    static PATTERNS: LazyLock<Vec<(&'static str, &'static LazyLock<Regex>)>> =
        LazyLock::new(|| {
            vec![
                (
                    "\"Boss worker\"",
                    lazy_re!(r"(?i)\bBoss\s+worker\b"),
                ),
                (
                    "\"the engine\"",
                    lazy_re!(r"(?i)\bthe\s+engine\b"),
                ),
                (
                    "\"the coordinator\"",
                    lazy_re!(r"(?i)\bthe\s+coordinator\b"),
                ),
                (
                    "\"cube workspace\"",
                    lazy_re!(r"(?i)\bcube\s+workspace\b"),
                ),
                (
                    "\"cube lease\"",
                    lazy_re!(r"(?i)\bcube\s+lease\b"),
                ),
                (
                    "\"work item\"",
                    lazy_re!(r"(?i)\bwork\s+item\b"),
                ),
                (
                    "\"execution id\"",
                    lazy_re!(r"(?i)\bexecution\s+id\b"),
                ),
                (
                    "\"PostToolUse\"",
                    lazy_re!(r"\bPostToolUse\b"),
                ),
                (
                    "\"PreToolUse\"",
                    lazy_re!(r"\bPreToolUse\b"),
                ),
            ]
        });
    &PATTERNS
}

// UUID pattern: standard lowercase hex form.
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").expect("uuid re")
});

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Evaluate `body` and `title` against `rules` (plus the always-on baked-in
/// defaults) and return an `EditorialDecision`.
///
/// `template_body` is the content of `.github/PULL_REQUEST_TEMPLATE.md` (or
/// `None` when unavailable / policy is `Off`). It is used only when
/// `rules.source.template_policy == Enforce`.
///
/// The function never panics on well-formed inputs. Regex errors from
/// `rules.compiled` patterns are already caught at `CompiledRules::compile`
/// time.
pub fn evaluate(
    body: &str,
    title: &str,
    rules: &CompiledRules,
    template_body: Option<&str>,
) -> EditorialDecision {
    let mut findings: Vec<Finding> = Vec::new();
    let new_body = apply_redactions(body, rules, &mut findings);
    let new_title = apply_redactions_to_title(title, rules, &mut findings);
    apply_template_check(&new_body, rules, template_body, &mut findings);

    let has_block = findings.iter().any(|f| {
        matches!(f.kind, FindingKind::Block | FindingKind::Template)
    });

    if findings.is_empty() {
        EditorialDecision::Allow
    } else if has_block {
        EditorialDecision::Block { findings }
    } else {
        // All findings are Redact-kind.
        EditorialDecision::Rewrite {
            body: new_body,
            title: new_title,
            findings,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: redaction pass
// ---------------------------------------------------------------------------

/// Apply all baked-in + user redaction rules to `text`, respecting markdown
/// code-fence and inline-code semantics. Populates `findings` for every
/// distinct named pattern that fired. Returns the rewritten text.
fn apply_redactions(
    text: &str,
    rules: &CompiledRules,
    findings: &mut Vec<Finding>,
) -> String {
    let segments = split_markdown_segments(text);
    let mut out = String::with_capacity(text.len());

    for seg in &segments {
        match seg {
            Segment::FencedCode(s) => out.push_str(s),
            Segment::InlineCode(s) => {
                // Only apply Rewrite patterns that match the *entire* span
                // content (backtick-delimited) — higher confidence bar.
                let content = inner_backtick_content(s);
                let mut replaced = false;
                for (name, re) in baked_in_rewrite_patterns() {
                    if re.is_match(content) && re.find(content).is_some_and(|m| {
                        m.start() == 0 && m.end() == content.len()
                    }) {
                        // Replace the whole inline-code span with empty.
                        record_finding(findings, FindingKind::Redact, name);
                        // Collapse: skip the span entirely (empty replacement).
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    // User patterns: same full-span bar.
                    for (re, replacement, kind) in &rules.compiled {
                        if *kind == RedactionKind::Rewrite
                            && re.is_match(content)
                            && re.find(content).is_some_and(|m| {
                                m.start() == 0 && m.end() == content.len()
                            })
                        {
                            record_finding(findings, FindingKind::Redact, "user redaction rule");
                            if replacement.is_empty() {
                                replaced = true;
                    } else {
                                out.push('`');
                                out.push_str(replacement);
                                out.push('`');
                                replaced = true;
                            }
                            break;
                        }
                    }
                }
                if !replaced {
                    out.push_str(s);
                }
            }
            Segment::Plain(s) => {
                let mut piece = s.to_string();
                // Baked-in Rewrite patterns.
                for (name, re) in baked_in_rewrite_patterns() {
                    if re.is_match(&piece) {
                        record_finding(findings, FindingKind::Redact, name);
                        piece = re.replace_all(&piece, "").into_owned();
                    }
                }
                // UUID-near-lease/cube (baked-in Rewrite).
                piece = redact_uuids_near_lease_cube(&piece, findings);
                // Baked-in Block patterns — mark but do NOT rewrite.
                for (name, re) in baked_in_block_patterns() {
                    if re.is_match(&piece) {
                        record_finding(findings, FindingKind::Block, name);
                    }
                }
                // User-configured Rewrite rules.
                for (re, replacement, kind) in &rules.compiled {
                    if *kind == RedactionKind::Rewrite && re.is_match(&piece) {
                        record_finding(findings, FindingKind::Redact, "user redaction rule");
                        piece = re.replace_all(&piece, replacement.as_str()).into_owned();
                    }
                }
                // User-configured Block rules — mark but do NOT rewrite.
                for (re, _, kind) in &rules.compiled {
                    if *kind == RedactionKind::Block && re.is_match(&piece) {
                        record_finding(findings, FindingKind::Block, "user block rule");
                    }
                }
                // Collapse multiple spaces left by redaction.
                out.push_str(&collapse_whitespace(&piece));
            }
        }
    }

    out
}

fn apply_redactions_to_title(
    title: &str,
    rules: &CompiledRules,
    findings: &mut Vec<Finding>,
) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    // Titles are plain text — no markdown segments.
    let mut piece = title.to_string();
    for (name, re) in baked_in_rewrite_patterns() {
        if re.is_match(&piece) {
            record_finding(findings, FindingKind::Redact, name);
            piece = re.replace_all(&piece, "").into_owned();
        }
    }
    piece = redact_uuids_near_lease_cube(&piece, findings);
    for (name, re) in baked_in_block_patterns() {
        if re.is_match(&piece) {
            record_finding(findings, FindingKind::Block, name);
        }
    }
    for (re, replacement, kind) in &rules.compiled {
        if *kind == RedactionKind::Rewrite && re.is_match(&piece) {
            record_finding(findings, FindingKind::Redact, "user redaction rule");
            piece = re.replace_all(&piece, replacement.as_str()).into_owned();
        }
    }
    for (re, _, kind) in &rules.compiled {
        if *kind == RedactionKind::Block && re.is_match(&piece) {
            record_finding(findings, FindingKind::Block, "user block rule");
        }
    }
    let result = collapse_whitespace(&piece);
    if result == title { None } else { Some(result) }
}

// ---------------------------------------------------------------------------
// Internal: template conformance check
// ---------------------------------------------------------------------------

fn apply_template_check(
    body: &str,
    rules: &CompiledRules,
    template_body: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    if rules.source.template_policy != TemplatePolicy::Enforce {
        return;
    }
    let Some(tmpl) = template_body else { return };
    let required = extract_headings(tmpl);
    let present = extract_headings(body);
    for heading in &required {
        if !present.contains(heading) {
            findings.push(Finding {
                kind: FindingKind::Template,
                description: format!("PR body is missing required template section: \"{}\"", heading),
            });
        }
    }
}

/// Extract all H2 / H3 heading text from a markdown body.
/// Returns lowercased, trimmed heading text for case-insensitive comparison.
fn extract_headings(text: &str) -> Vec<String> {
    let mut headings = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("### ") {
            headings.push(rest.trim().to_lowercase());
        } else if let Some(rest) = trimmed.strip_prefix("## ") {
            headings.push(rest.trim().to_lowercase());
        }
    }
    headings
}

// ---------------------------------------------------------------------------
// Internal: markdown segment splitter
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Segment<'a> {
    /// Content of a fenced code block, including the opening and closing fences.
    FencedCode(&'a str),
    /// Content of an inline code span, including the surrounding backticks.
    InlineCode(&'a str),
    /// Regular prose text.
    Plain(&'a str),
}

/// Split `text` into a sequence of [`Segment`]s. Fenced code blocks and
/// inline code spans are identified so callers can apply different rules.
///
/// This is intentionally a simple scanner — not a full CommonMark parser.
/// It handles the common cases that appear in worker-authored PR bodies:
/// triple-backtick and triple-tilde fences, and single-backtick inline spans.
fn split_markdown_segments(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut plain_start = 0usize;

    while i < len {
        // Detect fenced code block openings (``` or ~~~) at the start of a line.
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        if at_line_start {
            let fence_char = bytes[i];
            if fence_char == b'`' || fence_char == b'~' {
                // Count the fence length (minimum 3 identical chars).
                let fence_end = i + bytes[i..].iter().take_while(|&&c| c == fence_char).count();
                let fence_len = fence_end - i;
                if fence_len >= 3 {
                    // Flush any accumulated plain text.
                    if plain_start < i {
                        segments.push(Segment::Plain(&text[plain_start..i]));
                    }
                    // Find the closing fence (same char, same or greater length, own line).
                    let block_content_start = i;
                    // Skip to the next line.
                    let mut j = fence_end;
                    while j < len && bytes[j] != b'\n' {
                        j += 1;
                    }
                    if j < len {
                        j += 1; // skip \n
                    }
                    // Search for a closing fence line.
                    let close_idx = find_fence_close(text, j, fence_char, fence_len);
                    let block_end = close_idx.unwrap_or(len);
                    segments.push(Segment::FencedCode(&text[block_content_start..block_end]));
                    i = block_end;
                    plain_start = i;
                    continue;
                }
            }
        }

        // Detect inline code spans (single backtick).
        if bytes[i] == b'`' && (i == 0 || bytes[i - 1] != b'`') {
            // Multi-backtick inline spans (`` ` ``) - count opening backticks.
            let tick_count = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            if tick_count == 1 || tick_count == 2 {
                // Look for matching closing sequence.
                let span_start = i;
                let content_start = i + tick_count;
                let closing: &[u8] = if tick_count == 1 { b"`" } else { b"``" };
                if let Some(rel) = find_closing_ticks(&text[content_start..], closing) {
                    let span_end = content_start + rel + closing.len();
                    if plain_start < span_start {
                        segments.push(Segment::Plain(&text[plain_start..span_start]));
                    }
                    segments.push(Segment::InlineCode(&text[span_start..span_end]));
                    i = span_end;
                    plain_start = i;
                    continue;
                }
            }
        }

        i += 1;
    }

    if plain_start < len {
        segments.push(Segment::Plain(&text[plain_start..]));
    }
    segments
}

/// Find the position of the closing fence (starting search from `start_byte`).
/// Returns the index *after* the closing fence's newline if found.
fn find_fence_close(text: &str, start_byte: usize, fence_char: u8, min_len: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = start_byte;
    while i < len {
        // We're at a line start here (invariant maintained below).
        let fence_end = i + bytes[i..].iter().take_while(|&&c| c == fence_char).count();
        let fence_len = fence_end - i;
        if fence_len >= min_len {
            // Consume the rest of the line and the newline.
            let mut j = fence_end;
            while j < len && bytes[j] != b'\n' {
                j += 1;
            }
            if j < len {
                j += 1;
            }
            return Some(j);
        }
        // Advance to the next line.
        while i < len && bytes[i] != b'\n' {
            i += 1;
        }
        if i < len {
            i += 1;
        }
    }
    None
}

/// Find the byte offset of `closing` within `text`, returning the offset of
/// the first character of `closing`.
fn find_closing_ticks(text: &str, closing: &[u8]) -> Option<usize> {
    let bytes = text.as_bytes();
    if closing.len() > bytes.len() {
        return None;
    }
    for i in 0..=(bytes.len() - closing.len()) {
        if bytes[i..i + closing.len()] == *closing {
            return Some(i);
        }
    }
    None
}

/// Return the inner content of an inline code span (strips leading/trailing
/// backticks).
fn inner_backtick_content(span: &str) -> &str {
    
    (span.trim_start_matches('`').trim_end_matches('`')) as _
}

// ---------------------------------------------------------------------------
// Internal: UUID-near-lease/cube redaction
// ---------------------------------------------------------------------------

/// Strip UUIDs that appear within 40 characters of the words "lease" or
/// "cube" in either direction. Returns the rewritten text and populates
/// `findings` if any UUIDs were stripped.
fn redact_uuids_near_lease_cube(text: &str, findings: &mut Vec<Finding>) -> String {
    let mut result = text.to_string();
    // Collect match ranges before mutating.
    let mut to_remove: Vec<(usize, usize)> = Vec::new();

    for m in UUID_RE.find_iter(text) {
        let start = m.start();
        let end = m.end();
        let window_start = start.saturating_sub(40);
        let window_end = (end + 40).min(text.len());
        let window = &text[window_start..window_end];
        if window.contains("lease") || window.contains("cube") || window.contains("Cube") {
            to_remove.push((start, end));
        }
    }

    if to_remove.is_empty() {
        return result;
    }

    record_finding(findings, FindingKind::Redact, "cube/lease UUID");
    // Remove in reverse order to preserve indices.
    for (start, end) in to_remove.into_iter().rev() {
        result.replace_range(start..end, "");
    }
    collapse_whitespace(&result)
}

// ---------------------------------------------------------------------------
// Internal: helpers
// ---------------------------------------------------------------------------

/// Collapse sequences of two or more spaces (or space+newline artefacts left
/// by id-stripping) into a single space.
fn collapse_whitespace(s: &str) -> String {
    static MULTI_SPACE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"  +").expect("multi-space re"));
    static LEADING_SPACE_AFTER_NL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\n +\n").expect("space-between-nl re"));

    let s = LEADING_SPACE_AFTER_NL.replace_all(s, "\n\n");
    let s = MULTI_SPACE.replace_all(&s, " ");
    s.into_owned()
}

/// Add a finding for `kind` / `description` only if one with the same
/// description is not already present (de-duplicate repeated pattern matches).
fn record_finding(findings: &mut Vec<Finding>, kind: FindingKind, description: &str) {
    if !findings.iter().any(|f| f.description == description) {
        findings.push(Finding {
            kind,
            description: description.to_owned(),
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use boss_protocol::{EditorialRules, RedactionKind, RedactionRule, TemplatePolicy};

    use super::*;

    fn no_rules() -> CompiledRules {
        CompiledRules::compile(EditorialRules::default()).unwrap()
    }

    fn evaluate_simple(body: &str) -> EditorialDecision {
        evaluate(body, "", &no_rules(), None)
    }


    // -----------------------------------------------------------------------
    // Baked-in identifier redactions
    // -----------------------------------------------------------------------

    #[test]
    fn exec_id_in_body_is_redacted() {
        let body = "Fixes the bug introduced in exec_18b07a506d2518d0_1b.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new_body, findings, .. } => {
                assert!(!new_body.contains("exec_18b07a506d2518d0_1b"), "id must be removed");
                assert!(findings.iter().any(|f| f.description.contains("exec_")));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn proj_id_is_redacted() {
        let body = "Tracks project proj_18a2bb0000000000_ab.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("proj_"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn task_id_is_redacted() {
        let body = "Related: task_18a2000000000000_zz";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("task_"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn chg_id_is_redacted() {
        let body = "Change id: chg_8c8120badf6742d0b44be1002aebfb34";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("chg_"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn boss_exec_branch_is_redacted() {
        let body = "Branch boss/exec_18b07a506d2518d0_1b was used.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("boss/exec_"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn uuid_near_lease_is_redacted() {
        // "lease" alone (no "cube") so only the UUID-near-lease rule fires,
        // not the "cube lease" phrase-block rule.
        let body = "lease id: 48e14e9f-d591-4c73-b999-eab959c77134 was held.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("48e14e9f-d591-4c73-b999-eab959c77134"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn uuid_far_from_lease_is_not_redacted() {
        // UUID more than 40 chars away from any "lease"/"cube" reference.
        let body = "PR opened. 48e14e9f-d591-4c73-b999-eab959c77134 was the transaction id.";
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // Baked-in phrase blocks
    // -----------------------------------------------------------------------

    #[test]
    fn boss_worker_phrase_is_blocked() {
        let body = "This PR was created by a Boss worker running in the workspace.";
        match evaluate_simple(body) {
            EditorialDecision::Block { findings } => {
                assert!(findings.iter().any(|f| f.description.contains("Boss worker")));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn the_engine_phrase_is_blocked() {
        let body = "The engine spawned this session.";
        match evaluate_simple(body) {
            EditorialDecision::Block { findings } => {
                assert!(findings.iter().any(|f| f.description.contains("\"the engine\"")));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn the_coordinator_phrase_is_blocked() {
        let body = "Ask the coordinator for more context.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn cube_workspace_phrase_is_blocked() {
        let body = "The cube workspace is /workspaces/mono-agent-001.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn work_item_phrase_is_blocked() {
        let body = "This work item is complete.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn execution_id_phrase_is_blocked() {
        let body = "See the execution id in the logs.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn posttooluse_phrase_is_blocked() {
        let body = "The PostToolUse hook fired.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn pretooluse_phrase_is_blocked() {
        let body = "PreToolUse intercepted the call.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn clean_body_is_allowed() {
        let body = "## Summary\n\nFixes the login button alignment.";
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // Code-fence skipping (R2)
    // -----------------------------------------------------------------------

    #[test]
    fn exec_id_inside_fenced_block_is_not_redacted() {
        let body = "Example id format:\n\n```\nexec_18b07a506d2518d0_1b\n```\n\nSee above.";
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    #[test]
    fn exec_id_outside_fence_is_still_redacted() {
        let body = "This exec_18b07a506d2518d0_1b was inside prose, not a fence:\n\n```\nsafe\n```";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("exec_18b07a506d2518d0_1b"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn phrase_inside_fenced_block_is_not_blocked() {
        let body = "Debug output:\n\n~~~\nBoss worker session started\n~~~\n\nDone.";
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    #[test]
    fn tilde_fence_is_also_recognised() {
        let body = "~~~rust\nexec_18b07a506d2518d0_1b\n~~~\n";
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // Inline code span bar (R2)
    // -----------------------------------------------------------------------

    #[test]
    fn exec_id_as_whole_inline_span_is_redacted() {
        // The entire span content is the id.
        let body = "The id `exec_18b07a506d2518d0_1b` must not appear.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("exec_18b07a506d2518d0_1b"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn exec_id_inside_longer_inline_span_is_preserved() {
        // The span contains prose around the id — higher-confidence bar not met.
        let body = "See `the format exec_18b07a506d2518d0_1b like this` for details.";
        // The id is inside a span with surrounding text, so not a full-span match.
        // Result: Allow (the inline span protects it).
        assert_eq!(evaluate_simple(body), EditorialDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // Title redaction
    // -----------------------------------------------------------------------

    #[test]
    fn exec_id_in_title_is_redacted() {
        let title = "fix: close task exec_18b07a506d2518d0_1b";
        match evaluate("", title, &no_rules(), None) {
            EditorialDecision::Rewrite { title: Some(new_title), .. } => {
                assert!(!new_title.contains("exec_18b07a506d2518d0_1b"));
            }
            other => panic!("expected Rewrite with title, got {other:?}"),
        }
    }

    #[test]
    fn clean_title_produces_no_title_rewrite() {
        match evaluate("", "fix: correct alignment", &no_rules(), None) {
            EditorialDecision::Allow => {}
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Template missing-section detection (R4)
    // -----------------------------------------------------------------------

    fn enforce_rules() -> CompiledRules {
        let mut rules = EditorialRules::default();
        rules.template_policy = TemplatePolicy::Enforce;
        CompiledRules::compile(rules).unwrap()
    }

    #[test]
    fn template_enforce_missing_section_blocks() {
        let template = "## Summary\n\n## Test Plan\n";
        let body = "## Summary\n\nFixes the issue.";
        let rules = enforce_rules();
        match evaluate(body, "", &rules, Some(template)) {
            EditorialDecision::Block { findings } => {
                assert!(findings.iter().any(|f| {
                    f.kind == FindingKind::Template && f.description.contains("test plan")
                }));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn template_enforce_all_sections_present_allows() {
        let template = "## Summary\n\n## Test Plan\n";
        let body = "## Summary\n\nFixes the issue.\n\n## Test Plan\n\nManual smoke test.";
        let rules = enforce_rules();
        assert_eq!(evaluate(body, "", &rules, Some(template)), EditorialDecision::Allow);
    }

    #[test]
    fn template_enforce_h3_heading_detected() {
        let template = "### Steps to Reproduce\n";
        let body = "## Summary\n\nNo steps provided.";
        let rules = enforce_rules();
        match evaluate(body, "", &rules, Some(template)) {
            EditorialDecision::Block { findings } => {
                assert!(findings.iter().any(|f| f.kind == FindingKind::Template));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn template_off_does_not_check_sections() {
        let template = "## Summary\n\n## Test Plan\n";
        let body = "Prose with no headings.";
        // Default policy is Off.
        assert_eq!(evaluate(body, "", &no_rules(), Some(template)), EditorialDecision::Allow);
    }

    #[test]
    fn template_enforce_no_template_body_is_no_op() {
        // Policy = Enforce but no template supplied → no check, no findings.
        let rules = enforce_rules();
        assert_eq!(evaluate("## Summary\n\nOK.", "", &rules, None), EditorialDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // Empty config fires baked-in defaults
    // -----------------------------------------------------------------------

    #[test]
    fn empty_rules_still_fires_baked_in_exec_redaction() {
        let rules = CompiledRules::compile(EditorialRules::default()).unwrap();
        let body = "Trace: exec_18b07a506d2518d0_1b";
        assert!(matches!(evaluate(body, "", &rules, None), EditorialDecision::Rewrite { .. }));
    }

    #[test]
    fn empty_rules_still_fires_baked_in_phrase_block() {
        let rules = CompiledRules::compile(EditorialRules::default()).unwrap();
        let body = "The engine did the work.";
        assert!(matches!(evaluate(body, "", &rules, None), EditorialDecision::Block { .. }));
    }

    // -----------------------------------------------------------------------
    // User-configured redaction rules
    // -----------------------------------------------------------------------

    #[test]
    fn user_rewrite_rule_applied() {
        let rules = CompiledRules::compile(EditorialRules {
            redactions: vec![RedactionRule {
                pattern: r"\bACME-\d+\b".to_owned(),
                replacement: "[ticket]".to_owned(),
                kind: RedactionKind::Rewrite,
            }],
            ..Default::default()
        })
        .unwrap();
        let body = "Closes ACME-1234.";
        match evaluate(body, "", &rules, None) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(new.contains("[ticket]"), "replacement must appear: {new}");
                assert!(!new.contains("ACME-1234"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn user_block_rule_applied() {
        let rules = CompiledRules::compile(EditorialRules {
            redactions: vec![RedactionRule {
                pattern: r"\bconfidential\b".to_owned(),
                replacement: String::new(),
                kind: RedactionKind::Block,
            }],
            ..Default::default()
        })
        .unwrap();
        let body = "This is a confidential change.";
        match evaluate(body, "", &rules, None) {
            EditorialDecision::Block { findings } => {
                assert!(findings.iter().any(|f| f.description.contains("user block rule")));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn invalid_user_regex_is_caught_at_compile_time() {
        let result = CompiledRules::compile(EditorialRules {
            redactions: vec![RedactionRule {
                pattern: r"[invalid(regex".to_owned(),
                replacement: String::new(),
                kind: RedactionKind::Rewrite,
            }],
            ..Default::default()
        });
        assert!(result.is_err(), "invalid regex should fail at compile time");
    }

    // -----------------------------------------------------------------------
    // Block + Rewrite combined: Block wins
    // -----------------------------------------------------------------------

    #[test]
    fn block_finding_takes_precedence_over_rewrite() {
        // Body has both a redactable id and a blocking phrase.
        let body = "exec_18b07a506d2518d0_1b was created by a Boss worker.";
        match evaluate_simple(body) {
            EditorialDecision::Block { .. } => {}
            other => panic!("Block should win over Rewrite; got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Whitespace collapse
    // -----------------------------------------------------------------------

    #[test]
    fn id_removal_collapses_adjacent_spaces() {
        let body = "Branch  exec_18b07a506d2518d0_1b  pushed.";
        match evaluate_simple(body) {
            EditorialDecision::Rewrite { body: new, .. } => {
                assert!(!new.contains("  "), "double space should be collapsed: {new:?}");
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }
}
