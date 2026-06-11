use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

pub const DEFAULT_MAX_FIELDS: usize = 5;

pub fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().ok().is_some_and(|id| id == "test"))
}

/// Find the 1-based line number where `struct <name>` is declared, or `None`.
/// Handles a leading visibility modifier (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`).
pub fn struct_declaration_line(source: &str, struct_name: &str) -> Option<u32> {
    let search = format!("struct {struct_name}");
    for (i, line) in source.lines().enumerate() {
        let candidate = strip_visibility(line.trim_start());
        if let Some(after) = candidate.strip_prefix(&search)
            && (after.is_empty() || matches!(after.chars().next(), Some(' ' | '\t' | '<' | '{' | '(')))
        {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Strip a leading `pub` / `pub(...)` visibility modifier (and following whitespace) from
/// an already-`trim_start`ed line. Leaves the line untouched when there is no visibility
/// keyword (so `published` is not mistaken for `pub`).
pub fn strip_visibility(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    match rest.chars().next() {
        Some('(') => match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => line,
        },
        Some(c) if c.is_whitespace() => rest.trim_start(),
        _ => line,
    }
}

/// A pattern with no glob metacharacters resolves to a single concrete file path.
pub fn is_literal_path(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']', '{', '}', '!'])
}

pub fn parse_exclude_files(patterns: Option<&[String]>) -> Result<Option<GlobSet>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid `exclude_files` pattern: {pattern}"))?;
        builder.add(glob);
    }
    Ok(Some(
        builder.build().context("failed to compile `exclude_files` patterns")?,
    ))
}

/// Returns true if `path` is within `config_dir` and matches `globs` (relative to config_dir).
/// Files outside the config_dir subtree are never excluded.
pub fn is_excluded(path: &Path, globs: &GlobSet, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    globs.is_match(relative)
}
