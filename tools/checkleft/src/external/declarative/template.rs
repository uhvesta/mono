//! The `{{...}}` templating used inside declarative finding maps.
//!
//! A template is a sequence of literal text and `{{ref}}` substitutions. Four
//! ref kinds are supported:
//!
//! - `{{item.a.b.c}}` — a path into the selected JSON item (the "row").
//! - `{{input.file}}` — invocation context: the file the invocation ran on
//!   (per-file mode only). This is the load-bearing case for buildifier's lint
//!   pass, whose warning objects carry no filename — the path comes from context.
//! - `{{exit_code}}` — invocation context: the process exit code.
//! - `{{needs.<name>.invocation}}` — the human-readable invocation prefix for the
//!   named binary (e.g. `npx --yes prettier@3.8.4`). Populated at execution time
//!   from the resolved binary map so remediation strings stay in lockstep with the
//!   actual command that was run, including any per-repo version overrides.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde_json::Value;

/// A parsed template: an ordered list of literal/ref parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    /// Navigation path into the selected item, e.g. `["start", "line"]`.
    ItemPath(Vec<String>),
    InputFile,
    ExitCode,
    /// Human-readable invocation prefix for a named binary, e.g. `needs.prettier.invocation`
    /// → `"npx --yes prettier@3.8.4"`. Populated from the resolved binary map at render time.
    NeedsInvocation(String),
}

/// Invocation context available to every template render.
#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    /// The file the invocation ran on, if it ran per-file. `None` in batch mode.
    pub input_file: Option<&'a str>,
    pub exit_code: Option<i32>,
    /// Human-readable invocation strings keyed by binary name, e.g. `prettier` →
    /// `"npx --yes prettier@3.8.4"`. `None` when not available (e.g. in unit tests
    /// that drive transforms directly without going through the executor).
    pub needs_invocations: Option<&'a BTreeMap<String, String>>,
}

impl Template {
    /// Parse a template string. Errors on an unterminated `{{` or an unknown ref.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut rest = raw;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                parts.push(Part::Literal(rest[..open].to_owned()));
            }
            let after = &rest[open + 2..];
            let Some(close) = after.find("}}") else {
                bail!("unterminated `{{{{` in template `{raw}`");
            };
            let inner = after[..close].trim();
            parts.push(parse_ref(inner)?);
            rest = &after[close + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_owned()));
        }
        Ok(Self { parts })
    }

    /// Render against the selected `item` and invocation `context`.
    pub fn render(&self, item: &Value, context: RenderContext<'_>) -> Result<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::ItemPath(path) => {
                    let value = navigate(item, path).ok_or_else(|| {
                        anyhow::anyhow!("template ref `item.{}` not found in JSON item", path.join("."))
                    })?;
                    out.push_str(&value_to_string(value));
                }
                Part::InputFile => {
                    let file = context.input_file.ok_or_else(|| {
                        anyhow::anyhow!("template ref `input.file` is only available in per_file mode")
                    })?;
                    out.push_str(file);
                }
                Part::ExitCode => {
                    let code = context
                        .exit_code
                        .ok_or_else(|| anyhow::anyhow!("template ref `exit_code` has no value"))?;
                    out.push_str(&code.to_string());
                }
                Part::NeedsInvocation(name) => {
                    let invocations = context.needs_invocations.ok_or_else(|| {
                        anyhow::anyhow!("template ref `needs.{name}.invocation` is not available in this context")
                    })?;
                    let invocation = invocations.get(name.as_str()).ok_or_else(|| {
                        anyhow::anyhow!("template ref `needs.{name}.invocation`: no binary named `{name}` was resolved")
                    })?;
                    out.push_str(invocation);
                }
            }
        }
        Ok(out)
    }
}

fn parse_ref(inner: &str) -> Result<Part> {
    if inner == "input.file" {
        return Ok(Part::InputFile);
    }
    if inner == "exit_code" {
        return Ok(Part::ExitCode);
    }
    if let Some(path) = inner.strip_prefix("item.") {
        let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
        if segments.iter().any(|segment| segment.is_empty()) {
            bail!("malformed item ref `{{{{{inner}}}}}`");
        }
        return Ok(Part::ItemPath(segments));
    }
    if let Some(rest) = inner.strip_prefix("needs.") {
        if let Some(name) = rest.strip_suffix(".invocation") {
            if name.is_empty() || name.contains('.') {
                bail!("malformed needs ref `{{{{{inner}}}}}` — binary name must not be empty or contain dots");
            }
            return Ok(Part::NeedsInvocation(name.to_owned()));
        }
        bail!("unknown needs ref `{{{{{inner}}}}}` (only `needs.<name>.invocation` is supported)");
    }
    bail!(
        "unknown template ref `{{{{{inner}}}}}` (expected `item.*`, `input.file`, `exit_code`, or `needs.<name>.invocation`)"
    )
}

/// Walk `value` following object keys in `path`.
fn navigate<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
}

/// Stringify a JSON scalar the way a template expects: strings unquoted, numbers
/// and bools by their natural text. Composite values fall back to compact JSON.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(string) => string.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}
