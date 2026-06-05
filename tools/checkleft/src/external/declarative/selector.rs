//! A deliberately tiny jq-subset selector — just enough to express buildifier's
//! two transforms:
//!
//! - `.files[] | select(.formatted == false)` (format pass)
//! - `.files[].warnings[]` (lint pass)
//!
//! Grammar (informal):
//!
//! ```text
//! selector := segment ( "|" segment )*
//! segment  := path | "select(" path op literal ")"
//! path     := ( "." field "[]"? )+
//! op       := "=="
//! literal  := "true" | "false" | number | "\"" string "\""
//! ```
//!
//! Evaluation threads a working set of JSON values: `Field` projects into an
//! object key, `Iterate` flattens an array, `Select` filters by a comparison.
//! Anything richer (variable binding like `.files[] as $f | $f.warnings[]`,
//! arithmetic, `|=`, function calls) is intentionally unsupported — that
//! complexity is the seam where the wasm pure-function transform tier takes over.

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Project into an object key.
    Field(String),
    /// Flatten an array into its elements.
    Iterate,
    /// Keep values where `path == value`.
    Select { path: Vec<String>, value: Value },
}

impl Selector {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut steps = Vec::new();
        for segment in raw.split('|') {
            let segment = segment.trim();
            if segment.is_empty() {
                bail!("empty selector segment in `{raw}`");
            }
            if let Some(inner) = segment.strip_prefix("select(") {
                let inner = inner
                    .strip_suffix(')')
                    .ok_or_else(|| anyhow::anyhow!("unterminated `select(` in `{segment}`"))?;
                steps.push(parse_select(inner.trim())?);
            } else {
                parse_path_into(segment, &mut steps)?;
            }
        }
        if steps.is_empty() {
            bail!("selector `{raw}` produced no steps");
        }
        Ok(Self { steps })
    }

    /// Evaluate against `root`, returning the selected items (the "rows" each
    /// finding is projected from).
    pub fn select<'a>(&self, root: &'a Value) -> Vec<&'a Value> {
        let mut working: Vec<&Value> = vec![root];
        for step in &self.steps {
            working = match step {
                Step::Field(name) => working.iter().filter_map(|v| v.get(name)).collect(),
                Step::Iterate => working
                    .iter()
                    .filter_map(|v| v.as_array())
                    .flatten()
                    .collect(),
                Step::Select { path, value } => working
                    .into_iter()
                    .filter(|v| navigate(v, path) == Some(value))
                    .collect(),
            };
        }
        working
    }
}

/// Parse a `.a.b[].c` path into a sequence of `Field`/`Iterate` steps.
fn parse_path_into(segment: &str, steps: &mut Vec<Step>) -> Result<()> {
    let rest = segment
        .strip_prefix('.')
        .ok_or_else(|| anyhow::anyhow!("path segment `{segment}` must start with `.`"))?;
    // Split on `.` but keep trailing `[]` attached to each field.
    for raw_field in rest.split('.') {
        if raw_field.is_empty() {
            bail!("empty field in path segment `{segment}`");
        }
        let (name, iterate) = match raw_field.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (raw_field, false),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            bail!("invalid field name `{name}` in path segment `{segment}`");
        }
        steps.push(Step::Field(name.to_owned()));
        if iterate {
            steps.push(Step::Iterate);
        }
    }
    Ok(())
}

/// Parse `.a.b == literal` inside a `select(...)`.
fn parse_select(inner: &str) -> Result<Step> {
    let (lhs, rhs) = inner
        .split_once("==")
        .ok_or_else(|| anyhow::anyhow!("select expression `{inner}` must use `==`"))?;
    let lhs = lhs.trim();
    let path = lhs
        .strip_prefix('.')
        .ok_or_else(|| anyhow::anyhow!("select path `{lhs}` must start with `.`"))?;
    let path: Vec<String> = path.split('.').map(str::to_owned).collect();
    if path.iter().any(String::is_empty) {
        bail!("malformed select path `{lhs}`");
    }
    let value = parse_literal(rhs.trim())?;
    Ok(Step::Select { path, value })
}

fn parse_literal(raw: &str) -> Result<Value> {
    match raw {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        _ => {
            if let Some(quoted) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                return Ok(Value::String(quoted.to_owned()));
            }
            if let Ok(number) = raw.parse::<i64>() {
                return Ok(Value::Number(number.into()));
            }
            bail!("unsupported select literal `{raw}` (expected bool, null, integer, or \"string\")")
        }
    }
}

fn navigate<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
}
