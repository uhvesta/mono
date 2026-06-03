use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, bail};
use regex::Regex;

use crate::path::validate_relative_path;

static IFCHANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^LINT\.IfChange(?:\(([A-Za-z0-9_-]+)\))?$").expect("valid ifchange regex")
});
static THENCHANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^LINT\.ThenChange\(([^)]+)\)$").expect("valid thenchange regex"));
static LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("valid label regex"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIfChangeFile {
    pub path: PathBuf,
    pub blocks: Vec<ParsedIfChangeBlock>,
    labels: BTreeMap<String, usize>,
}

impl ParsedIfChangeFile {
    pub fn block_by_label(&self, label: &str) -> Option<&ParsedIfChangeBlock> {
        self.labels
            .get(label)
            .and_then(|index| self.blocks.get(*index))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIfChangeBlock {
    pub source_label: Option<String>,
    pub ifchange_line: usize,
    pub thenchange_line: usize,
    pub body_range: Option<LineRange>,
    pub target: ThenChangeTarget,
}

impl ParsedIfChangeBlock {
    pub fn full_range(&self) -> LineRange {
        LineRange {
            start: self.ifchange_line,
            end: self.thenchange_line,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThenChangeTarget {
    File { path: PathBuf },
    Block { path: PathBuf, label: String },
}

pub fn parse_ifchange_file(path: &Path, contents: &str) -> Result<ParsedIfChangeFile> {
    validate_relative_path(path)?;

    let mut blocks = Vec::new();
    let mut labels = BTreeMap::new();
    let mut current: Option<OpenIfChangeBlock> = None;

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line_number = line_index + 1;
        let Some(directive) = parse_directive(raw_line)? else {
            continue;
        };

        match directive {
            Directive::IfChange { label } => {
                if current.is_some() {
                    bail!(
                        "{}:{}: nested `LINT.IfChange` blocks are not supported",
                        path.display(),
                        line_number
                    );
                }
                if let Some(label) = label.as_ref()
                    && labels.contains_key(label) {
                        bail!(
                            "{}:{}: duplicate `LINT.IfChange({label})` label",
                            path.display(),
                            line_number
                        );
                    }
                current = Some(OpenIfChangeBlock {
                    source_label: label,
                    ifchange_line: line_number,
                });
            }
            Directive::ThenChange { target } => {
                let Some(open) = current.take() else {
                    bail!(
                        "{}:{}: `LINT.ThenChange(...)` must close a preceding `LINT.IfChange` block",
                        path.display(),
                        line_number
                    );
                };

                let body_range = if line_number > open.ifchange_line + 1 {
                    Some(LineRange {
                        start: open.ifchange_line + 1,
                        end: line_number - 1,
                    })
                } else {
                    None
                };

                let block_index = blocks.len();
                if let Some(label) = open.source_label.as_ref() {
                    labels.insert(label.clone(), block_index);
                }
                blocks.push(ParsedIfChangeBlock {
                    source_label: open.source_label,
                    ifchange_line: open.ifchange_line,
                    thenchange_line: line_number,
                    body_range,
                    target,
                });
            }
        }
    }

    if let Some(open) = current {
        bail!(
            "{}:{}: `LINT.IfChange` block is missing a closing `LINT.ThenChange(...)`",
            path.display(),
            open.ifchange_line
        );
    }

    Ok(ParsedIfChangeFile {
        path: path.to_path_buf(),
        blocks,
        labels,
    })
}

#[derive(Debug)]
struct OpenIfChangeBlock {
    source_label: Option<String>,
    ifchange_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Directive {
    IfChange { label: Option<String> },
    ThenChange { target: ThenChangeTarget },
}

fn parse_directive(line: &str) -> Result<Option<Directive>> {
    let text = normalize_directive_text(line);
    if let Some(captures) = IFCHANGE_RE.captures(text) {
        let label = captures.get(1).map(|value| value.as_str().to_owned());
        return Ok(Some(Directive::IfChange { label }));
    }

    let Some(captures) = THENCHANGE_RE.captures(text) else {
        return Ok(None);
    };
    let target = parse_thenchange_target(captures.get(1).expect("target").as_str())?;
    Ok(Some(Directive::ThenChange { target }))
}

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

fn parse_thenchange_target(raw: &str) -> Result<ThenChangeTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("`LINT.ThenChange(...)` target must not be empty");
    }

    let (path_text, label) = match trimmed.rsplit_once(':') {
        Some((path, label)) if LABEL_RE.is_match(label.trim()) => (path.trim(), Some(label.trim())),
        _ => (trimmed, None),
    };

    let path = PathBuf::from(path_text);
    validate_relative_path(&path)?;

    Ok(match label {
        Some(label) => ThenChangeTarget::Block {
            path,
            label: label.to_owned(),
        },
        None => ThenChangeTarget::File { path },
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LineRange, ThenChangeTarget, parse_ifchange_file};

    #[test]
    fn parses_unlabeled_block_targeting_file() {
        let parsed = parse_ifchange_file(
            Path::new("backend/version.rs"),
            r#"
// LINT.IfChange
const VERSION: &str = "v1";
// LINT.ThenChange(tools/release/version.txt)
"#,
        )
        .expect("parse");

        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label, None);
        assert_eq!(
            parsed.blocks[0].body_range,
            Some(LineRange { start: 3, end: 3 })
        );
        assert_eq!(
            parsed.blocks[0].target,
            ThenChangeTarget::File {
                path: Path::new("tools/release/version.txt").to_path_buf(),
            }
        );
    }

    #[test]
    fn parses_labeled_block_targeting_labeled_block() {
        let parsed = parse_ifchange_file(
            Path::new("backend/api/user.proto"),
            r#"
# LINT.IfChange(schema)
message User {}
# LINT.ThenChange(frontend/src/types.ts:user_schema)
"#,
        )
        .expect("parse");

        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label.as_deref(), Some("schema"));
        assert_eq!(
            parsed.blocks[0].target,
            ThenChangeTarget::Block {
                path: Path::new("frontend/src/types.ts").to_path_buf(),
                label: "user_schema".to_owned(),
            }
        );
        assert!(parsed.block_by_label("schema").is_some());
    }

    #[test]
    fn ignores_prose_mentions_of_ifchange() {
        let parsed = parse_ifchange_file(
            Path::new("docs/guide.md"),
            "Use LINT.IfChange in comments, not in prose.\n",
        )
        .expect("parse");

        assert!(parsed.blocks.is_empty());
    }

    #[test]
    fn rejects_duplicate_labels() {
        let error = parse_ifchange_file(
            Path::new("docs/guide.md"),
            r#"
// LINT.IfChange(shared)
// LINT.ThenChange(other/file.md)
// LINT.IfChange(shared)
// LINT.ThenChange(other/file.md)
"#,
        )
        .expect_err("must fail");

        assert!(
            error
                .to_string()
                .contains("duplicate `LINT.IfChange(shared)` label")
        );
    }

    #[test]
    fn rejects_nested_ifchange_blocks() {
        let error = parse_ifchange_file(
            Path::new("docs/guide.md"),
            r#"
// LINT.IfChange
// LINT.IfChange
// LINT.ThenChange(other/file.md)
"#,
        )
        .expect_err("must fail");

        assert!(error.to_string().contains("nested `LINT.IfChange` blocks"));
    }

    #[test]
    fn rejects_missing_thenchange() {
        let error = parse_ifchange_file(
            Path::new("docs/guide.md"),
            r#"
// LINT.IfChange(orphan)
still open
"#,
        )
        .expect_err("must fail");

        assert!(
            error
                .to_string()
                .contains("missing a closing `LINT.ThenChange(...)`")
        );
    }

    #[test]
    fn rejects_thenchange_without_ifchange() {
        let error = parse_ifchange_file(
            Path::new("docs/guide.md"),
            "// LINT.ThenChange(other/file.md)\n",
        )
        .expect_err("must fail");

        assert!(
            error
                .to_string()
                .contains("must close a preceding `LINT.IfChange` block")
        );
    }

    #[test]
    fn rejects_invalid_thenchange_target() {
        let error = parse_ifchange_file(
            Path::new("docs/guide.md"),
            r#"
// LINT.IfChange
// LINT.ThenChange(../escape.md)
"#,
        )
        .expect_err("must fail");

        assert!(error.to_string().contains("path traversal is not allowed"));
    }
}
