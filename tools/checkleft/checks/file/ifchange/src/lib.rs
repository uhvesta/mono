//! Checkleft check: require a companion change when a coupled surface changes.
//!
//! This is the Component Model wasm check `file/ifchange`. It is
//! the generalization of the former `ifchange-thenchange` built-in and the
//! former native `api-breaking-surface` check. Both expressed the same rule —
//! *"when region/surface X changes, companion Y must also change"* — through
//! two different coupling-declaration mechanisms; this check supports both at
//! once.
//!
//! ## Two ways to declare a coupling
//!
//! 1. **In-source markers** (`LINT.IfChange` / `LINT.ThenChange`). Code-declared
//!    coupling between specific regions/files. Always active — no config needed.
//!    This is the former `ifchange-thenchange` behavior, at full parity
//!    (including enforcement on deleted files and removed-marker scenarios via
//!    base-revision content supplied through [`ChangeSet::base_file_content`]).
//!
//! 2. **Config globs** (`trigger_globs` / `required_globs`). Policy-declared
//!    coupling scoped by path globs: if any changed file matches a coupling's
//!    `trigger_globs` but no changed file matches its `required_globs`, every
//!    trigger file is flagged. This is the former `api-breaking-surface`
//!    behavior, now a config of this generic check.
//!
//! The two mechanisms are independent and additive: a single instance can rely
//! on markers, on glob couplings, or on both.
//!
//! ## Multi-target `LINT.ThenChange`
//!
//! A single `LINT.ThenChange` can list multiple comma-separated targets.
//! Every listed target must be updated in the same change when the guarded
//! region changes:
//!
//! ```text
//! LINT.ThenChange(fileA, fileB)
//! LINT.ThenChange(fileA:region, fileB, path/to/fileC:other-region)
//! ```
//!
//! Each entry in the list uses the same forms accepted for a single target
//! (bare file, `file:region`). Whitespace around entries is ignored; empty
//! entries (e.g. trailing commas) are also ignored. A violation names only the
//! specific targets that were not updated.
//!
//! ## Supported comment styles (markers)
//!
//! The `LINT.IfChange` / `LINT.ThenChange` directives are recognized when preceded
//! by any of: `//`, `#`, `--`, `;`, `/*`, `*`, `<!--` (and `*/` / `-->` suffixes
//! are stripped). This covers most common source languages.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "trigger_globs": ["backend/blob/src/v3/**"],
//!   "required_globs": ["docs/backend.md", "docs/product-specs/**"],
//!   "message": "Potential backend API surface change without docs update.",
//!   "remediation": "Update docs/backend.md or a relevant product spec in this PR."
//! }
//! ```
//!
//! The flat `trigger_globs` / `required_globs` form declares a single coupling.
//! For multiple couplings in one instance, use the `couplings` array, each entry
//! carrying its own `trigger_globs` / `required_globs` / `message` / `remediation`.
//! With no config, only the in-source marker mechanism is active.

use std::collections::{BTreeMap, BTreeSet};

use checkleft_check_sdk::{
    ChangeKind, ChangeSet, ChangedFile, CheckInput, DiffHunk, Finding, Location, Severity, check,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Require a companion change when a coupled surface changes.
#[check(
    name = "file/ifchange",
    description = "requires a companion change (marked region/file or glob-matched surface) to change together",
    severity = error,
    access_scope = whole_repo
)]
pub fn file_ifchange_check(input: CheckInput) -> Vec<Finding> {
    run(&input)
}

/// Runs both coupling mechanisms and concatenates their findings.
fn run(input: &CheckInput) -> Vec<Finding> {
    let mut findings = marker_findings(&input.changeset);
    findings.extend(glob_findings(input));
    findings
}

// ══════════════════════════════════════════════════════════════════════════════
// Mechanism 1 — in-source LINT.IfChange / LINT.ThenChange markers
// ══════════════════════════════════════════════════════════════════════════════

// ── Parsing types ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct IfChangeFile {
    blocks: Vec<IfChangeBlock>,
    label_map: BTreeMap<String, usize>,
}

impl IfChangeFile {
    fn block_by_label(&self, label: &str) -> Option<&IfChangeBlock> {
        self.label_map.get(label).and_then(|i| self.blocks.get(*i))
    }
}

#[derive(Clone, Debug)]
struct IfChangeBlock {
    source_label: Option<String>,
    ifchange_line: usize,
    thenchange_line: usize,
    targets: Vec<ThenChangeTarget>,
}

#[derive(Clone, Debug)]
enum ThenChangeTarget {
    File { path: String },
    Block { path: String, label: String },
}

// ── Marker-mode driver ─────────────────────────────────────────────────────────

fn marker_findings(changeset: &ChangeSet) -> Vec<Finding> {
    let analyses: Vec<FileAnalysis> = changeset
        .changed_files
        .iter()
        .map(|f| analyze_file(f, changeset))
        .collect();

    let mut findings = Vec::new();

    for analysis in &analyses {
        findings.extend(analysis.parse_findings.iter().cloned());
    }

    let mut emitted_keys: BTreeSet<String> = BTreeSet::new();

    for analysis in &analyses {
        if !analysis.parse_findings.is_empty() {
            continue;
        }
        for block in &analysis.touched_blocks {
            let key = format!("{}:{}:{}", analysis.path, block.ifchange_line, block.thenchange_line);
            if !emitted_keys.insert(key) {
                continue;
            }

            for target in &block.targets {
                let status = target_status(target, changeset, &analyses);
                match status {
                    TargetStatus::Satisfied => {}
                    TargetStatus::MissingFile => findings.push(broken_target_finding(
                        &analysis.path,
                        block,
                        target,
                        TargetStatus::MissingFile,
                    )),
                    TargetStatus::MissingLabel => findings.push(broken_target_finding(
                        &analysis.path,
                        block,
                        target,
                        TargetStatus::MissingLabel,
                    )),
                    TargetStatus::NotChanged => findings.push(broken_target_finding(
                        &analysis.path,
                        block,
                        target,
                        TargetStatus::NotChanged,
                    )),
                }
            }
        }
    }

    findings
}

// ── Per-file analysis ─────────────────────────────────────────────────────────

struct FileAnalysis {
    path: String,
    touched_blocks: Vec<IfChangeBlock>,
    parse_findings: Vec<Finding>,
}

fn analyze_file(changed_file: &ChangedFile, changeset: &ChangeSet) -> FileAnalysis {
    let path = changed_file.path.clone();

    // Gap 1: deleted files — read the base revision to find IfChange blocks.
    // All blocks in a deleted file are considered "touched" (the whole file is gone).
    if changed_file.kind == ChangeKind::Deleted {
        let Some(base_content) = changeset.base_file_content(&path) else {
            // No base content available (e.g. no base revision configured).
            return FileAnalysis {
                path,
                touched_blocks: vec![],
                parse_findings: vec![],
            };
        };
        let parsed = match parse_ifchange_file(&path, base_content) {
            Ok(p) => p,
            Err(_) => {
                return FileAnalysis {
                    path,
                    touched_blocks: vec![],
                    parse_findings: vec![],
                };
            }
        };
        return FileAnalysis {
            path,
            touched_blocks: parsed.blocks,
            parse_findings: vec![],
        };
    }

    // A file that cannot be read as UTF-8 text (e.g. a binary asset) cannot carry
    // LINT markers, so skip it rather than emitting an error. This keeps
    // glob-coupling-only configs — which still run marker analysis over every
    // changed file — from flagging binary or unreadable changes, preserving the
    // former api-breaking-surface behavior of never reading file contents.
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return FileAnalysis {
                path,
                touched_blocks: vec![],
                parse_findings: vec![],
            };
        }
    };

    let parsed = match parse_ifchange_file(&path, &content) {
        Ok(p) => p,
        Err(msg) => {
            let finding = Finding {
                severity: Severity::Error,
                message: msg,
                location: Some(Location {
                    path: path.clone(),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            };
            return FileAnalysis {
                path,
                touched_blocks: vec![],
                parse_findings: vec![finding],
            };
        }
    };

    let diff = changeset.file_diffs.iter().find(|d| d.path == path);

    let mut touched_blocks: Vec<IfChangeBlock> = parsed
        .blocks
        .iter()
        .filter(|block| {
            diff.is_some_and(|d| {
                d.hunks
                    .iter()
                    .any(|hunk| hunk_touches_range_new(hunk, block.ifchange_line, block.thenchange_line))
            })
        })
        .cloned()
        .collect();

    // Gap 2: removed markers on modified files — check if any IfChange block
    // present in the base revision was removed from the current file while the
    // guarded region was touched (using OLD diff coordinates).
    if changed_file.kind == ChangeKind::Modified
        && let Some(base_content) = changeset.base_file_content(&path)
        && let Ok(base_parsed) = parse_ifchange_file(&path, base_content)
    {
        for base_block in &base_parsed.blocks {
            let still_present = parsed.blocks.iter().any(|b| {
                b.source_label == base_block.source_label && targets_list_match(&b.targets, &base_block.targets)
            });
            if !still_present {
                let was_touched = diff.is_some_and(|d| {
                    d.hunks
                        .iter()
                        .any(|hunk| hunk_touches_range_old(hunk, base_block.ifchange_line, base_block.thenchange_line))
                });
                if was_touched {
                    touched_blocks.push(base_block.clone());
                }
            }
        }
    }

    FileAnalysis {
        path,
        touched_blocks,
        parse_findings: vec![],
    }
}

// ── Target status ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetStatus {
    Satisfied,
    MissingFile,
    MissingLabel,
    NotChanged,
}

fn target_status(target: &ThenChangeTarget, changeset: &ChangeSet, analyses: &[FileAnalysis]) -> TargetStatus {
    match target {
        ThenChangeTarget::File { path } => {
            // A target that was itself changed (even deleted) satisfies the constraint.
            if file_changed(changeset, path) {
                return TargetStatus::Satisfied;
            }
            if !std::path::Path::new(path).exists() {
                return TargetStatus::MissingFile;
            }
            TargetStatus::NotChanged
        }
        ThenChangeTarget::Block { path, label } => {
            // A target file that was changed (even deleted) satisfies the constraint
            // at the file level; we skip the block-existence check for deleted targets.
            if file_changed(changeset, path) {
                // File was changed — check if the specific block was touched.
                let Some(target_analysis) = analyses.iter().find(|a| a.path == *path) else {
                    // File changed but no analysis (e.g. it was deleted) — satisfied.
                    return TargetStatus::Satisfied;
                };
                if target_analysis
                    .touched_blocks
                    .iter()
                    .any(|b| b.source_label.as_deref() == Some(label))
                {
                    return TargetStatus::Satisfied;
                }
                // File changed but the specific block was not touched.
                return TargetStatus::NotChanged;
            }
            if !std::path::Path::new(path).exists() {
                return TargetStatus::MissingFile;
            }
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return TargetStatus::NotChanged,
            };
            let parsed = match parse_ifchange_file(path, &content) {
                Ok(p) => p,
                Err(_) => return TargetStatus::NotChanged,
            };
            if parsed.block_by_label(label).is_none() {
                return TargetStatus::MissingLabel;
            }
            let Some(target_analysis) = analyses.iter().find(|a| a.path == *path) else {
                return TargetStatus::NotChanged;
            };
            if target_analysis
                .touched_blocks
                .iter()
                .any(|b| b.source_label.as_deref() == Some(label))
            {
                TargetStatus::Satisfied
            } else {
                TargetStatus::NotChanged
            }
        }
    }
}

fn file_changed(changeset: &ChangeSet, target_path: &str) -> bool {
    changeset
        .changed_files
        .iter()
        .any(|f| f.path == target_path || f.old_path.as_deref().is_some_and(|op| op == target_path))
}

// ── Target matching ───────────────────────────────────────────────────────────

fn targets_match(a: &ThenChangeTarget, b: &ThenChangeTarget) -> bool {
    match (a, b) {
        (ThenChangeTarget::File { path: pa }, ThenChangeTarget::File { path: pb }) => pa == pb,
        (ThenChangeTarget::Block { path: pa, label: la }, ThenChangeTarget::Block { path: pb, label: lb }) => {
            pa == pb && la == lb
        }
        _ => false,
    }
}

fn targets_list_match(a: &[ThenChangeTarget], b: &[ThenChangeTarget]) -> bool {
    // Order-insensitive: reordering targets in an existing ThenChange is not
    // treated as marker removal.
    a.len() == b.len() && a.iter().all(|ta| b.iter().any(|tb| targets_match(ta, tb)))
}

// ── Hunk / range overlap ──────────────────────────────────────────────────────

fn hunk_touches_range_new(hunk: &DiffHunk, range_start: usize, range_end: usize) -> bool {
    hunk_touches_range(hunk.new_start as usize, hunk.new_lines as usize, range_start, range_end)
}

fn hunk_touches_range_old(hunk: &DiffHunk, range_start: usize, range_end: usize) -> bool {
    hunk_touches_range(hunk.old_start as usize, hunk.old_lines as usize, range_start, range_end)
}

fn hunk_touches_range(start: usize, len: usize, range_start: usize, range_end: usize) -> bool {
    if len == 0 {
        return start >= range_start && start <= range_end.saturating_add(1);
    }
    let end = start.saturating_add(len.saturating_sub(1));
    start <= range_end && end >= range_start
}

// ── Finding construction ──────────────────────────────────────────────────────

fn broken_target_finding(
    source_path: &str,
    block: &IfChangeBlock,
    target: &ThenChangeTarget,
    status: TargetStatus,
) -> Finding {
    Finding {
        severity: Severity::Error,
        message: format_violation_message(source_path, block, target, status),
        location: Some(Location {
            path: source_path.to_owned(),
            line: Some(block.ifchange_line as u32),
            column: Some(1),
        }),
        remediations: vec![
            "Update the linked file or block in the same change, or bypass the check with a documented reason."
                .to_owned(),
        ],
        suggested_fix: None,
    }
}

fn format_violation_message(
    source_path: &str,
    block: &IfChangeBlock,
    target: &ThenChangeTarget,
    status: TargetStatus,
) -> String {
    let source_clause = match &block.source_label {
        Some(label) => format!("when changing `{label}` in `{source_path}`"),
        None => format!("when changing `{source_path}`"),
    };
    match (target, status) {
        (ThenChangeTarget::File { path }, TargetStatus::NotChanged) => {
            format!("{source_clause}, you must also change `{path}`")
        }
        (ThenChangeTarget::File { path }, TargetStatus::MissingFile) => {
            format!("{source_clause}, required companion `{path}` does not exist in the current tree")
        }
        (ThenChangeTarget::Block { path, label }, TargetStatus::NotChanged) => {
            format!("{source_clause}, you must also change `{path}` (`{label}`)")
        }
        (ThenChangeTarget::Block { path, label }, TargetStatus::MissingLabel) => {
            format!(
                "{source_clause}, required companion block `{label}` in `{path}` does not exist in the current tree"
            )
        }
        (ThenChangeTarget::Block { path, .. }, TargetStatus::MissingFile) => {
            format!("{source_clause}, required companion `{path}` does not exist in the current tree")
        }
        (_, TargetStatus::Satisfied) => unreachable!("Satisfied findings are filtered before reaching here"),
        _ => unreachable!("unexpected target/status combination"),
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

fn parse_ifchange_file(path: &str, contents: &str) -> Result<IfChangeFile, String> {
    let mut blocks = Vec::new();
    let mut label_map: BTreeMap<String, usize> = BTreeMap::new();
    let mut current: Option<(Option<String>, usize)> = None;

    for (line_idx, raw_line) in contents.lines().enumerate() {
        let line_number = line_idx + 1;
        let text = normalize_directive_text(raw_line);

        if let Some(maybe_label) = parse_ifchange_directive(text) {
            if current.is_some() {
                return Err(format!(
                    "{path}:{line_number}: nested `LINT.IfChange` blocks are not supported"
                ));
            }
            if let Some(ref label) = maybe_label
                && label_map.contains_key(label.as_str())
            {
                return Err(format!(
                    "{path}:{line_number}: duplicate `LINT.IfChange({label})` label"
                ));
            }
            current = Some((maybe_label, line_number));
        } else if let Some(raw_target) = parse_thenchange_directive(text) {
            let Some((source_label, ifchange_line)) = current.take() else {
                return Err(format!(
                    "{path}:{line_number}: `LINT.ThenChange(...)` must close a preceding `LINT.IfChange` block"
                ));
            };
            let targets = parse_thenchange_targets(raw_target).map_err(|e| format!("{path}:{line_number}: {e}"))?;
            let block_index = blocks.len();
            if let Some(ref label) = source_label {
                label_map.insert(label.clone(), block_index);
            }
            blocks.push(IfChangeBlock {
                source_label,
                ifchange_line,
                thenchange_line: line_number,
                targets,
            });
        }
    }

    if let Some((_, open_line)) = current {
        return Err(format!(
            "{path}:{open_line}: `LINT.IfChange` block is missing a closing `LINT.ThenChange(...)`"
        ));
    }

    Ok(IfChangeFile { blocks, label_map })
}

/// Strip comment markers from a source line and return the core directive text.
fn normalize_directive_text(line: &str) -> &str {
    let mut text = line.trim();
    for prefix in ["//", "#", "--", ";", "/*", "*", "<!--"] {
        if let Some(stripped) = text.strip_prefix(prefix) {
            text = stripped.trim_start();
            break;
        }
    }
    for suffix in ["*/", "-->"] {
        if let Some(stripped) = text.strip_suffix(suffix) {
            text = stripped.trim_end();
        }
    }
    text
}

/// Recognizes `LINT.IfChange` (returns `Some(None)`) or `LINT.IfChange(label)`
/// (returns `Some(Some(label))`). Returns `None` for any other text.
fn parse_ifchange_directive(text: &str) -> Option<Option<String>> {
    if text == "LINT.IfChange" {
        return Some(None);
    }
    let rest = text.strip_prefix("LINT.IfChange(")?;
    let label = rest.strip_suffix(')')?;
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(Some(label.to_owned()))
}

/// Recognizes `LINT.ThenChange(<target>)` and returns the raw target string,
/// or `None` for any other text.
fn parse_thenchange_directive(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("LINT.ThenChange(")?;
    let target = rest.strip_suffix(')')?;
    if target.is_empty() || target.contains(')') {
        return None;
    }
    Some(target)
}

fn parse_thenchange_targets(raw: &str) -> Result<Vec<ThenChangeTarget>, String> {
    let targets: Result<Vec<_>, _> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_thenchange_target)
        .collect();
    let targets = targets?;
    if targets.is_empty() {
        return Err("`LINT.ThenChange(...)` must have at least one non-empty target".to_owned());
    }
    Ok(targets)
}

fn parse_thenchange_target(raw: &str) -> Result<ThenChangeTarget, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`LINT.ThenChange(...)` target must not be empty".to_owned());
    }

    let (path_text, label) = match trimmed.rfind(':') {
        Some(pos) => {
            let maybe_label = trimmed[pos + 1..].trim();
            if !maybe_label.is_empty()
                && maybe_label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                (&trimmed[..pos], Some(maybe_label))
            } else {
                (trimmed, None)
            }
        }
        None => (trimmed, None),
    };

    validate_path(path_text.trim())?;

    Ok(match label {
        Some(l) => ThenChangeTarget::Block {
            path: path_text.trim().to_owned(),
            label: l.to_owned(),
        },
        None => ThenChangeTarget::File {
            path: path_text.trim().to_owned(),
        },
    })
}

fn validate_path(path_text: &str) -> Result<(), String> {
    let p = std::path::Path::new(path_text);
    if p.is_absolute() {
        return Err(format!(
            "path `{path_text}` is absolute: only relative paths are allowed"
        ));
    }
    for component in p.components() {
        use std::path::Component;
        if matches!(component, Component::ParentDir) {
            return Err(format!("path traversal is not allowed in `{path_text}`"));
        }
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// Mechanism 2 — config glob couplings (former api-breaking-surface)
// ══════════════════════════════════════════════════════════════════════════════

/// Top-level configuration. The flat `trigger_globs` / `required_globs` fields
/// declare a single coupling (compatible with the former `api-breaking-surface`
/// config); `couplings` declares any number of additional couplings.
#[derive(Debug, Deserialize, Default)]
struct CompanionConfig {
    #[serde(default)]
    trigger_globs: Vec<String>,
    #[serde(default)]
    required_globs: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
    #[serde(default)]
    couplings: Vec<Coupling>,
}

/// One glob-based coupling: a trigger surface and the companion it requires.
#[derive(Debug, Deserialize, Default)]
struct Coupling {
    #[serde(default)]
    trigger_globs: Vec<String>,
    #[serde(default)]
    required_globs: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

impl CompanionConfig {
    /// Flatten the top-level coupling (if any) plus the explicit `couplings` list.
    fn into_couplings(self) -> Vec<Coupling> {
        let mut out = Vec::new();
        if !self.trigger_globs.is_empty() || !self.required_globs.is_empty() {
            out.push(Coupling {
                trigger_globs: self.trigger_globs,
                required_globs: self.required_globs,
                message: self.message,
                remediation: self.remediation,
            });
        }
        out.extend(self.couplings);
        out
    }
}

fn glob_findings(input: &CheckInput) -> Vec<Finding> {
    // '{}' (absent config) deserializes fine to CompanionConfig::default() —
    // that means no couplings, which is correct for marker-only mode.
    // Any other parse error means the config is genuinely malformed; surface it
    // rather than silently disabling glob enforcement.
    let cfg: CompanionConfig = match input.config() {
        Ok(c) => c,
        Err(e) => return vec![config_error_finding(&format!("invalid config JSON: {e}"))],
    };

    cfg.into_couplings()
        .iter()
        .flat_map(|coupling| evaluate_coupling(coupling, &input.changeset))
        .collect()
}

fn evaluate_coupling(coupling: &Coupling, changeset: &ChangeSet) -> Vec<Finding> {
    if coupling.trigger_globs.is_empty() {
        // An intended-but-broken coupling: has required_globs/message/remediation
        // configured but no trigger_globs to fire on. Emit a config error so
        // the misconfiguration is visible rather than silently doing nothing.
        if !coupling.required_globs.is_empty() || coupling.message.is_some() || coupling.remediation.is_some() {
            return vec![config_error_finding(
                "`trigger_globs` must be non-empty when a coupling is configured",
            )];
        }
        return vec![];
    }
    // Misconfiguration: a trigger with no required companion can never be
    // satisfied. The former native check hard-errored here; the wasm check
    // surfaces it as an error finding (it cannot abort the whole run).
    if coupling.required_globs.is_empty() {
        return vec![config_error_finding(
            "`required_globs` must be set when `trigger_globs` is set",
        )];
    }

    let trigger = match compile_globs(&coupling.trigger_globs) {
        Ok(g) => g,
        Err(msg) => return vec![config_error_finding(&format!("invalid `trigger_globs`: {msg}"))],
    };
    let required = match compile_globs(&coupling.required_globs) {
        Ok(g) => g,
        Err(msg) => return vec![config_error_finding(&format!("invalid `required_globs`: {msg}"))],
    };

    let mut trigger_files = Vec::new();
    let mut required_updated = false;
    for changed_file in &changeset.changed_files {
        // Deleted files neither satisfy nor trigger the requirement, matching the
        // former api-breaking-surface behavior.
        if changed_file.kind == ChangeKind::Deleted {
            continue;
        }
        if required.is_match(&changed_file.path) {
            required_updated = true;
        }
        if trigger.is_match(&changed_file.path) {
            trigger_files.push(changed_file.path.clone());
        }
    }

    if trigger_files.is_empty() || required_updated {
        return vec![];
    }

    let message = coupling.message.clone().unwrap_or_else(default_companion_message);
    let remediation = coupling
        .remediation
        .clone()
        .unwrap_or_else(default_companion_remediation);

    trigger_files
        .into_iter()
        .map(|path| Finding {
            severity: Severity::Error,
            message: message.clone(),
            location: Some(Location {
                path,
                line: None,
                column: None,
            }),
            remediations: vec![remediation.clone()],
            suggested_fix: None,
        })
        .collect()
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| format!("`{pattern}`: {e}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|e| format!("{e}"))
}

fn config_error_finding(detail: &str) -> Finding {
    Finding {
        severity: Severity::Error,
        message: format!("ifchange config error: {detail}"),
        location: None,
        remediations: vec!["Fix the check configuration in the CHECKS file.".to_owned()],
        suggested_fix: None,
    }
}

fn default_companion_message() -> String {
    "a file matching `trigger_globs` changed, but no companion file matching `required_globs` was updated in the \
     same change"
        .to_owned()
}

fn default_companion_remediation() -> String {
    "Update a companion file matching the configured `required_globs` in the same change, or bypass the check with a \
     documented reason."
        .to_owned()
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check
// alongside the other preinstalled checks into a single multiplexed component.
// That deduplicates the shared wasm runtime baseline (std/alloc/SDK/wit-bindgen)
// across all preinstalled checks instead of duplicating it per check.

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput, DiffHunk, FileDiff};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Serialize CWD changes so parallel tests don't interfere.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_changeset(changed_files: Vec<(&str, ChangeKind)>, diffs: Vec<(&str, Vec<DiffHunk>)>) -> ChangeSet {
        make_changeset_with_base(changed_files, diffs, vec![])
    }

    fn make_changeset_with_base(
        changed_files: Vec<(&str, ChangeKind)>,
        diffs: Vec<(&str, Vec<DiffHunk>)>,
        base_files: Vec<(&str, &str)>,
    ) -> ChangeSet {
        use checkleft_check_sdk::BaseFile;
        ChangeSet {
            changed_files: changed_files
                .into_iter()
                .map(|(path, kind)| ChangedFile {
                    path: path.to_owned(),
                    kind,
                    old_path: None,
                })
                .collect(),
            file_diffs: diffs
                .into_iter()
                .map(|(path, hunks)| FileDiff {
                    path: path.to_owned(),
                    hunks,
                })
                .collect(),
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
            base_files: base_files
                .into_iter()
                .map(|(path, content)| BaseFile {
                    path: path.to_owned(),
                    content: content.to_owned(),
                })
                .collect(),
        }
    }

    /// A hunk touching `new_start` for `new_lines` lines (representing added lines).
    fn hunk_new(new_start: u32, new_lines: u32) -> DiffHunk {
        DiffHunk {
            old_start: 0,
            old_lines: 0,
            new_start,
            new_lines,
            added_lines: new_lines,
            removed_lines: 0,
        }
    }

    fn run_check(changeset: ChangeSet) -> Vec<Finding> {
        let input = CheckInput::__from_parts(changeset, "{}".to_owned());
        file_ifchange_check(input)
    }

    fn run_with_config(changeset: ChangeSet, config_json: &str) -> Vec<Finding> {
        let input = CheckInput::__from_parts(changeset, config_json.to_owned());
        file_ifchange_check(input)
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Marker-mode tests (parity with the former ifchange-thenchange built-in)
    // ══════════════════════════════════════════════════════════════════════════

    // ── File-target tests ─────────────────────────────────────────────────────

    #[test]
    fn passes_when_source_and_target_change_together() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("frontend/schema.txt"), "schema view v2\n").unwrap();

        // Hunk covers line 2 (the body line inside the IfChange block at lines 1-3).
        let cs = make_changeset(
            vec![
                ("backend/schema.txt", ChangeKind::Modified),
                ("frontend/schema.txt", ChangeKind::Modified),
            ],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn fails_when_linked_target_file_does_not_change() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("frontend/schema.txt"), "schema view v1\n").unwrap();

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("frontend/schema.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("you must also change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn fails_when_linked_target_file_does_not_exist() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(frontend/missing.txt)\n",
        )
        .unwrap();
        // frontend/missing.txt is intentionally NOT created.

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("does not exist in the current tree"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn no_finding_when_block_not_touched_by_diff() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // File has an IfChange block on lines 1-3 and some other content.
        // The hunk covers line 5 (outside the block range 1-3).
        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\nextra line\nchanged line\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(5, 1)])], // hunk is outside block (lines 1-3)
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "block not touched — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn deleted_file_with_no_base_content_produces_no_findings() {
        // Without base content in the changeset, a deleted file is skipped.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        // a.txt is deleted (not on disk), no base_files provided.
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Deleted)],
            vec![("a.txt", vec![hunk_new(1, 3)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "deleted file with no base content must produce no findings; got {findings:?}"
        );
    }

    #[test]
    fn deleted_file_with_ifchange_markers_is_flagged() {
        // When base content is available for a deleted file and it contained
        // LINT.IfChange blocks, the check flags missing target updates.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // b.txt still exists (target of a.txt's block) but was not changed.
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        let base_a_txt = "// LINT.IfChange\ncontent here\n// LINT.ThenChange(b.txt)\n";
        let cs = make_changeset_with_base(
            vec![("a.txt", ChangeKind::Deleted)],
            vec![("a.txt", vec![hunk_new(1, 3)])],
            vec![("a.txt", base_a_txt)],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("b.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("you must also change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn deleted_file_with_ifchange_markers_passes_when_target_also_changed() {
        // If the target file was also changed, the constraint is satisfied.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // b.txt also changed in the same changeset.
        fs::write(dir.path().join("b.txt"), "updated content\n").unwrap();

        let base_a_txt = "// LINT.IfChange\ncontent here\n// LINT.ThenChange(b.txt)\n";
        let cs = make_changeset_with_base(
            vec![("a.txt", ChangeKind::Deleted), ("b.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(1, 3)]), ("b.txt", vec![hunk_new(1, 1)])],
            vec![("a.txt", base_a_txt)],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "target was changed — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn removed_markers_on_modified_file_is_flagged() {
        // Gap 2: a change edits the guarded region AND removes its LINT markers.
        // The check must detect this via base-revision content and OLD hunk coords.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // Current version of a.txt has no markers (they were removed).
        fs::write(dir.path().join("a.txt"), "changed content\n").unwrap();
        // b.txt exists but was not changed.
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        // Base version had markers at lines 1-3, content at line 2.
        let base_a_txt = "// LINT.IfChange\ncontent here\n// LINT.ThenChange(b.txt)\n";

        // Hunk removes old lines 1-3 and replaces with 1 new line.
        // old_start=1, old_lines=3 → touches block [1, 3] in OLD coords.
        let cs = make_changeset_with_base(
            vec![("a.txt", ChangeKind::Modified)],
            vec![(
                "a.txt",
                vec![DiffHunk {
                    old_start: 1,
                    old_lines: 3,
                    new_start: 1,
                    new_lines: 1,
                    added_lines: 1,
                    removed_lines: 3,
                }],
            )],
            vec![("a.txt", base_a_txt)],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            1,
            "expected 1 finding for removed markers; got {findings:?}"
        );
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("b.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("you must also change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn removed_markers_passes_when_target_also_changed() {
        // If markers are removed AND the target was updated, no finding.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // Current version of a.txt has no markers.
        fs::write(dir.path().join("a.txt"), "changed content\n").unwrap();
        // b.txt was also changed.
        fs::write(dir.path().join("b.txt"), "updated\n").unwrap();

        let base_a_txt = "// LINT.IfChange\ncontent here\n// LINT.ThenChange(b.txt)\n";
        let cs = make_changeset_with_base(
            vec![("a.txt", ChangeKind::Modified), ("b.txt", ChangeKind::Modified)],
            vec![
                (
                    "a.txt",
                    vec![DiffHunk {
                        old_start: 1,
                        old_lines: 3,
                        new_start: 1,
                        new_lines: 1,
                        added_lines: 1,
                        removed_lines: 3,
                    }],
                ),
                ("b.txt", vec![hunk_new(1, 1)]),
            ],
            vec![("a.txt", base_a_txt)],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "target was changed — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn markers_still_present_in_modified_file_not_double_flagged() {
        // If markers are present in both base and current, they should only be
        // checked via the new-coord path; the base check must not add duplicates.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // Same markers in base and current.
        let content = "// LINT.IfChange\ncontent changed\n// LINT.ThenChange(b.txt)\n";
        fs::write(dir.path().join("a.txt"), content).unwrap();
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        let base_a_txt = "// LINT.IfChange\ncontent old\n// LINT.ThenChange(b.txt)\n";
        let cs = make_changeset_with_base(
            vec![("a.txt", ChangeKind::Modified)],
            vec![(
                "a.txt",
                vec![DiffHunk {
                    old_start: 2,
                    old_lines: 1,
                    new_start: 2,
                    new_lines: 1,
                    added_lines: 1,
                    removed_lines: 1,
                }],
            )],
            vec![("a.txt", base_a_txt)],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        // Should get exactly one finding (from the new-coord path), not two.
        assert_eq!(findings.len(), 1, "must not double-flag; got {findings:?}");
    }

    // ── Block-target tests ────────────────────────────────────────────────────

    #[test]
    fn passes_when_linked_target_block_changes() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=2\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .unwrap();

        let cs = make_changeset(
            vec![
                ("backend/schema.txt", ChangeKind::Modified),
                ("frontend/schema.txt", ChangeKind::Modified),
            ],
            vec![
                ("backend/schema.txt", vec![hunk_new(2, 1)]),
                ("frontend/schema.txt", vec![hunk_new(2, 1)]),
            ],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "both blocks touched — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn fails_when_linked_target_block_does_not_change() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=1\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .unwrap();

        // Only backend changes; frontend file exists but is not in the changeset.
        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert!(
            findings[0].message.contains("frontend/schema.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(findings[0].message.contains("view"), "message: {}", findings[0].message);
        assert!(
            findings[0].message.contains("you must also change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn reports_missing_target_label() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        // frontend/schema.txt exists but has no "view" label.
        fs::write(dir.path().join("frontend/schema.txt"), "render=1\n").unwrap();

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("frontend/schema.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(findings[0].message.contains("view"), "message: {}", findings[0].message);
        assert!(
            findings[0].message.contains("does not exist"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn finding_location_points_to_ifchange_line() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "prefix line\n// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b content\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(3, 1)])], // hunk on line 3 (body between lines 2 and 4)
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        let loc = findings[0].location.as_ref().expect("finding must have location");
        assert_eq!(loc.path, "a.txt");
        assert_eq!(loc.line, Some(2), "location must point at the IfChange line (line 2)");
        assert_eq!(loc.column, Some(1));
    }

    #[test]
    fn finding_message_contains_remediation() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b content\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            !findings[0].remediations.is_empty(),
            "finding must carry at least one remediation"
        );
    }

    // ── Deduplication ─────────────────────────────────────────────────────────

    #[test]
    fn duplicate_block_emitted_only_once() {
        // If both the "current" and some edge path would emit for the same block,
        // the key-based deduplication must suppress the duplicate.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        // Two hunks both touching the same block (lines 1-3).
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(1, 1), hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            1,
            "same block must not be emitted twice; got {findings:?}"
        );
    }

    // ── Parse error tests ─────────────────────────────────────────────────────

    #[test]
    fn reports_parse_error_for_nested_ifchange() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("bad.txt"),
            "// LINT.IfChange\n// LINT.IfChange\n// LINT.ThenChange(other.txt)\n",
        )
        .unwrap();

        let cs = make_changeset(
            vec![("bad.txt", ChangeKind::Modified)],
            vec![("bad.txt", vec![hunk_new(1, 3)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("nested"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn reports_parse_error_for_orphan_thenchange() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("bad.txt"), "// LINT.ThenChange(other.txt)\n").unwrap();

        let cs = make_changeset(
            vec![("bad.txt", ChangeKind::Modified)],
            vec![("bad.txt", vec![hunk_new(1, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("must close a preceding"),
            "message: {}",
            findings[0].message
        );
    }

    // ── Parsing unit tests (no IO) ────────────────────────────────────────────

    #[test]
    fn parse_unlabeled_block_targeting_file() {
        let parsed = parse_ifchange_file(
            "backend/version.rs",
            "// LINT.IfChange\nconst VERSION: &str = \"v1\";\n// LINT.ThenChange(tools/release/version.txt)\n",
        )
        .expect("parse");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label, None);
        assert_eq!(parsed.blocks[0].ifchange_line, 1);
        assert_eq!(parsed.blocks[0].thenchange_line, 3);
        assert_eq!(parsed.blocks[0].targets.len(), 1);
        match &parsed.blocks[0].targets[0] {
            ThenChangeTarget::File { path } => assert_eq!(path, "tools/release/version.txt"),
            other => panic!(
                "expected File target; got {other:?}",
                other = std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn parse_labeled_block_targeting_labeled_block() {
        let parsed = parse_ifchange_file(
            "backend/api/user.proto",
            "# LINT.IfChange(schema)\nmessage User {}\n# LINT.ThenChange(frontend/src/types.ts:user_schema)\n",
        )
        .expect("parse");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label.as_deref(), Some("schema"));
        assert_eq!(parsed.blocks[0].targets.len(), 1);
        match &parsed.blocks[0].targets[0] {
            ThenChangeTarget::Block { path, label } => {
                assert_eq!(path, "frontend/src/types.ts");
                assert_eq!(label, "user_schema");
            }
            other => panic!(
                "expected Block target; got {other:?}",
                other = std::mem::discriminant(other)
            ),
        }
        assert!(parsed.block_by_label("schema").is_some());
    }

    #[test]
    fn parse_ignores_prose_mentions_of_ifchange() {
        let parsed =
            parse_ifchange_file("docs/guide.md", "Use LINT.IfChange in comments, not in prose.\n").expect("parse");
        assert!(parsed.blocks.is_empty());
    }

    #[test]
    fn parse_rejects_duplicate_labels() {
        let err = parse_ifchange_file(
            "docs/guide.md",
            "// LINT.IfChange(shared)\n// LINT.ThenChange(other/file.md)\n// LINT.IfChange(shared)\n// LINT.ThenChange(other/file.md)\n",
        )
        .expect_err("must fail");
        assert!(err.contains("duplicate `LINT.IfChange(shared)` label"), "err: {err}");
    }

    #[test]
    fn parse_rejects_nested_ifchange_blocks() {
        let err = parse_ifchange_file(
            "docs/guide.md",
            "// LINT.IfChange\n// LINT.IfChange\n// LINT.ThenChange(other/file.md)\n",
        )
        .expect_err("must fail");
        assert!(err.contains("nested `LINT.IfChange` blocks"), "err: {err}");
    }

    #[test]
    fn parse_rejects_missing_thenchange() {
        let err =
            parse_ifchange_file("docs/guide.md", "// LINT.IfChange(orphan)\nstill open\n").expect_err("must fail");
        assert!(err.contains("missing a closing `LINT.ThenChange(...)`"), "err: {err}");
    }

    #[test]
    fn parse_rejects_thenchange_without_ifchange() {
        let err = parse_ifchange_file("docs/guide.md", "// LINT.ThenChange(other/file.md)\n").expect_err("must fail");
        assert!(
            err.contains("must close a preceding `LINT.IfChange` block"),
            "err: {err}"
        );
    }

    #[test]
    fn parse_rejects_invalid_thenchange_target() {
        let err = parse_ifchange_file("docs/guide.md", "// LINT.IfChange\n// LINT.ThenChange(../escape.md)\n")
            .expect_err("must fail");
        assert!(err.contains("path traversal is not allowed"), "err: {err}");
    }

    #[test]
    fn parse_recognizes_all_comment_styles() {
        for (prefix, suffix) in [
            ("// ", ""),
            ("# ", ""),
            ("-- ", ""),
            ("; ", ""),
            ("/* ", " */"),
            ("* ", ""),
            ("<!-- ", " -->"),
        ] {
            let content = format!("{prefix}LINT.IfChange{suffix}\nline\n{prefix}LINT.ThenChange(target.txt){suffix}\n");
            let parsed = parse_ifchange_file("file.txt", &content)
                .unwrap_or_else(|e| panic!("parse failed for prefix `{prefix}`: {e}"));
            assert_eq!(
                parsed.blocks.len(),
                1,
                "expected 1 block for prefix `{prefix}`; got {}",
                parsed.blocks.len()
            );
        }
    }

    // ── Hunk overlap unit tests ────────────────────────────────────────────────

    #[test]
    fn hunk_touching_first_line_of_block() {
        assert!(hunk_touches_range(1, 1, 1, 3));
    }

    #[test]
    fn hunk_touching_last_line_of_block() {
        assert!(hunk_touches_range(3, 1, 1, 3));
    }

    #[test]
    fn hunk_before_block_does_not_touch() {
        assert!(!hunk_touches_range(0, 0, 1, 3));
    }

    #[test]
    fn hunk_after_block_does_not_touch() {
        assert!(!hunk_touches_range(4, 1, 1, 3));
    }

    #[test]
    fn zero_len_hunk_at_block_boundary() {
        // An insertion at line 2 (between lines 1 and 2) touches range [1,3].
        assert!(hunk_touches_range(2, 0, 1, 3));
    }

    #[test]
    fn zero_len_hunk_beyond_block() {
        // An insertion at line 5 does not touch range [1,3].
        assert!(!hunk_touches_range(5, 0, 1, 3));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Glob-coupling tests (parity with the former api-breaking-surface)
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn glob_flags_trigger_change_without_required_update() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"]}"#,
        );
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert_eq!(
            findings[0].location.as_ref().map(|l| l.path.as_str()),
            Some("backend/blob/src/v3/auth.rs")
        );
    }

    #[test]
    fn glob_passes_when_required_file_is_updated() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(
            vec![
                ("backend/blob/src/v3/auth.rs", ChangeKind::Modified),
                ("docs/backend.md", ChangeKind::Modified),
            ],
            vec![],
        );
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"]}"#,
        );
        assert!(
            findings.is_empty(),
            "required companion updated — no finding; got {findings:?}"
        );
    }

    #[test]
    fn glob_ignores_changes_outside_trigger_globs() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v2/fencer.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/app.rs", "backend/blob/src/v2/mod.rs"], "required_globs": ["docs/backend.md"]}"#,
        );
        assert!(findings.is_empty(), "no trigger matched — no finding; got {findings:?}");
    }

    #[test]
    fn glob_custom_message_and_remediation_are_used() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"], "message": "API surface changed", "remediation": "update docs/backend.md"}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "API surface changed");
        assert_eq!(findings[0].remediations, vec!["update docs/backend.md".to_owned()]);
    }

    #[test]
    fn glob_deleted_trigger_file_does_not_fire() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Deleted)], vec![]);
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"]}"#,
        );
        assert!(findings.is_empty(), "deleted trigger must not fire; got {findings:?}");
    }

    #[test]
    fn glob_multiple_couplings_evaluated_independently() {
        let _guard = CWD_LOCK.lock().unwrap();
        // First coupling fires (backend changed, no docs); second does not (no frontend change).
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(
            cs,
            r#"{
                "couplings": [
                    {"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"]},
                    {"trigger_globs": ["frontend/**"], "required_globs": ["docs/frontend.md"]}
                ]
            }"#,
        );
        assert_eq!(
            findings.len(),
            1,
            "only the first coupling should fire; got {findings:?}"
        );
    }

    #[test]
    fn glob_config_with_trigger_but_no_required_reports_config_error() {
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(cs, r#"{"trigger_globs": ["backend/blob/src/v3/**"]}"#);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("config error"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("required_globs"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn no_config_means_marker_mode_only() {
        // With empty config and no markers, there are no findings (and no config error).
        let _guard = CWD_LOCK.lock().unwrap();
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(cs, "{}");
        assert!(
            findings.is_empty(),
            "no markers + no globs → no findings; got {findings:?}"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Combined + alias behavior
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn marker_and_glob_findings_both_emitted() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // Marker coupling that is unsatisfied (target b.txt not changed).
        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "unchanged\n").unwrap();
        fs::create_dir_all(dir.path().join("backend/blob/src/v3")).unwrap();
        fs::write(dir.path().join("backend/blob/src/v3/auth.rs"), "fn f() {}\n").unwrap();

        // a.txt's block touched (marker fires) AND a v3 backend file changed with
        // no docs companion (glob fires).
        let cs = make_changeset(
            vec![
                ("a.txt", ChangeKind::Modified),
                ("backend/blob/src/v3/auth.rs", ChangeKind::Modified),
            ],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_with_config(
            cs,
            r#"{"trigger_globs": ["backend/blob/src/v3/**"], "required_globs": ["docs/backend.md"]}"#,
        );

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            2,
            "expected one marker + one glob finding; got {findings:?}"
        );
    }

    #[test]
    fn file_ifchange_check_runs_marker_mechanism() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // Set up on-disk files with an unsatisfied LINT marker (b.txt not changed).
        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "unchanged\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let ifchange_findings = file_ifchange_check(CheckInput::__from_parts(cs, "{}".to_owned()));
        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(ifchange_findings.len(), 1, "file/ifchange must run marker mode");
    }

    #[test]
    fn malformed_glob_config_reports_config_error() {
        let _guard = CWD_LOCK.lock().unwrap();
        // trigger_globs given as a string instead of an array — valid JSON but
        // wrong type, so serde deserialization fails.
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(cs, r#"{"trigger_globs": "not-an-array"}"#);
        assert_eq!(
            findings.len(),
            1,
            "malformed config must produce a config-error finding; got {findings:?}"
        );
        assert!(
            findings[0].message.contains("config error"),
            "expected config error message; got: {}",
            findings[0].message
        );
    }

    #[test]
    fn required_globs_without_trigger_globs_reports_config_error() {
        let _guard = CWD_LOCK.lock().unwrap();
        // An intended-but-broken coupling: has required_globs but no trigger_globs.
        let cs = make_changeset(vec![("backend/blob/src/v3/auth.rs", ChangeKind::Modified)], vec![]);
        let findings = run_with_config(cs, r#"{"required_globs": ["docs/backend.md"]}"#);
        assert_eq!(
            findings.len(),
            1,
            "required_globs with no trigger_globs must produce a config-error finding; got {findings:?}"
        );
        assert!(
            findings[0].message.contains("config error"),
            "expected config error message; got: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("trigger_globs"),
            "error must mention trigger_globs; got: {}",
            findings[0].message
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Multi-target ThenChange tests
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn multi_target_both_updated_passes() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt, c.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        fs::write(dir.path().join("c.txt"), "c\n").unwrap();

        let cs = make_changeset(
            vec![
                ("a.txt", ChangeKind::Modified),
                ("b.txt", ChangeKind::Modified),
                ("c.txt", ChangeKind::Modified),
            ],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "both targets updated — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn multi_target_only_first_updated_reports_second_missing() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt, c.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        fs::write(dir.path().join("c.txt"), "c\n").unwrap();

        // Only b.txt is updated; c.txt is not.
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified), ("b.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            1,
            "expected 1 finding for missing c.txt; got {findings:?}"
        );
        assert!(
            findings[0].message.contains("c.txt"),
            "finding must name the missing target c.txt; message: {}",
            findings[0].message
        );
        assert!(
            !findings[0].message.contains("b.txt"),
            "finding must not mention the satisfied target b.txt; message: {}",
            findings[0].message
        );
    }

    #[test]
    fn multi_target_neither_updated_reports_both_missing() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt, c.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        fs::write(dir.path().join("c.txt"), "c\n").unwrap();

        // Neither b.txt nor c.txt is updated.
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            2,
            "expected 2 findings (one per missing target); got {findings:?}"
        );
        let messages: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("b.txt")),
            "one finding must name b.txt; messages: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("c.txt")),
            "one finding must name c.txt; messages: {messages:?}"
        );
    }

    #[test]
    fn multi_target_whitespace_variants_parsed_correctly() {
        // ThenChange(A,B) and ThenChange( A , B ) are both valid.
        let no_space =
            parse_ifchange_file("a.txt", "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt,c.txt)\n").unwrap();
        assert_eq!(no_space.blocks[0].targets.len(), 2);

        let with_space = parse_ifchange_file(
            "a.txt",
            "// LINT.IfChange\ncontent\n// LINT.ThenChange( b.txt , c.txt )\n",
        )
        .unwrap();
        assert_eq!(with_space.blocks[0].targets.len(), 2);
        match &with_space.blocks[0].targets[0] {
            ThenChangeTarget::File { path } => assert_eq!(path, "b.txt"),
            other => panic!("expected File; got {other:?}", other = std::mem::discriminant(other)),
        }
        match &with_space.blocks[0].targets[1] {
            ThenChangeTarget::File { path } => assert_eq!(path, "c.txt"),
            other => panic!("expected File; got {other:?}", other = std::mem::discriminant(other)),
        }
    }

    #[test]
    fn multi_target_trailing_comma_ignored() {
        // A trailing comma produces an empty entry that is silently dropped.
        let parsed =
            parse_ifchange_file("a.txt", "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt,c.txt,)\n").unwrap();
        assert_eq!(
            parsed.blocks[0].targets.len(),
            2,
            "trailing comma must not add an extra target"
        );
    }

    #[test]
    fn multi_target_mixed_file_and_block_targets() {
        let parsed = parse_ifchange_file(
            "a.txt",
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt, c.txt:my-label)\n",
        )
        .unwrap();
        assert_eq!(parsed.blocks[0].targets.len(), 2);
        match &parsed.blocks[0].targets[0] {
            ThenChangeTarget::File { path } => assert_eq!(path, "b.txt"),
            other => panic!("expected File; got {other:?}", other = std::mem::discriminant(other)),
        }
        match &parsed.blocks[0].targets[1] {
            ThenChangeTarget::Block { path, label } => {
                assert_eq!(path, "c.txt");
                assert_eq!(label, "my-label");
            }
            other => panic!("expected Block; got {other:?}", other = std::mem::discriminant(other)),
        }
    }

    #[test]
    fn single_target_regression_unchanged() {
        // A single-entry ThenChange still works exactly as before.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            1,
            "single target not updated → one finding; got {findings:?}"
        );
        assert!(
            findings[0].message.contains("b.txt"),
            "message: {}",
            findings[0].message
        );
    }
}
