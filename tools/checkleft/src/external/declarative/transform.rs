//! Declarative transforms: the projection DSL that turns an invocation's
//! `(stdout, exit_code, file-it-ran-on)` into `Vec<Finding>`.
//!
//! Two strategies are implemented:
//!
//! - `json` — a [`Selector`] locates issue rows and a [`FindingTemplate`] projects
//!   each into a [`Finding`] (enough for buildifier).
//! - `passthrough` — the invoked binary already emits checkleft findings JSON
//!   (`{"findings":[…]}`) on stdout, so the transform is the identity: parse and
//!   return them. This is how the old `exec-v1` tier folds into the declarative
//!   runtime — a custom binary "ships its own transform" by emitting findings
//!   directly, modelled as `{ invoke: <binary>, transform: passthrough }`.
//!
//! The [`Transform`] enum is the strategy slot: `regex` (parse stdout lines) and
//! `sarif` (map SARIF results) would each add a variant here and reuse the same
//! `apply(stdout, exit_code, input_file) → Vec<Finding>` signature. Computed
//! transforms that need real logic are where the wasm pure-function tier begins.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::output::{Finding, Location, Severity};

use super::selector::Selector;
use super::template::{RenderContext, Template};

/// Sentinel JSON item used when rendering templates that have no JSON item context
/// (e.g. linelist). Any `{{item.*}}` ref resolves to `None` → runtime error, which
/// correctly rejects item-path refs in non-JSON transforms.
const NO_JSON_ITEM: &Value = &Value::Null;

/// A declared projection strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transform {
    Json(JsonTransform),
    /// Identity: the binary already emitted checkleft findings JSON on stdout.
    Passthrough,
    /// Line-list: each non-empty stdout line is a file path with a violation.
    /// Used with tools like `rustfmt --check -l` that print one file per line.
    LineList(LineListTransform),
    // Future: Regex(RegexTransform), Sarif(SarifTransform).
}

impl Transform {
    /// Returns true when this transform contains a `SeverityTemplate::Dynamic`
    /// finding — i.e., the transform itself controls per-finding severity and the
    /// runner should not flatten it to a single default when no policy override is set.
    pub fn uses_dynamic_severity(&self) -> bool {
        match self {
            Transform::Json(json) => matches!(json.finding.severity, SeverityTemplate::Dynamic(_)),
            Transform::Passthrough | Transform::LineList(_) => false,
        }
    }

    /// Project an invocation's output into findings. `input_file` is the file the
    /// invocation ran on (per-file mode) or `None` (batch mode). `needs_invocations`
    /// maps binary names to their human-readable invocation strings for template
    /// rendering (e.g. `"prettier"` → `"npx --yes prettier@3.8.4"`); pass `None`
    /// when not available (e.g. in unit tests that bypass the executor).
    pub fn apply(
        &self,
        stdout: &[u8],
        exit_code: Option<i32>,
        input_file: Option<&str>,
        needs_invocations: Option<&BTreeMap<String, String>>,
    ) -> Result<Vec<Finding>> {
        match self {
            Transform::Json(json) => json.apply(stdout, exit_code, input_file, needs_invocations),
            Transform::Passthrough => passthrough(stdout),
            Transform::LineList(ll) => ll.apply(stdout, exit_code, input_file, needs_invocations),
        }
    }
}

/// The `passthrough` strategy: the binary emits a checkleft findings document
/// (`{"findings":[…]}`) on stdout; return those findings unchanged. `exit_code`
/// and `input_file` are unused — exit semantics already gated whether we got here.
fn passthrough(stdout: &[u8]) -> Result<Vec<Finding>> {
    let document: PassthroughDocument = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "declarative passthrough transform could not parse tool stdout as a checkleft \
             findings document (`{{\"findings\":[…]}}`); raw stdout: {:?}",
            String::from_utf8_lossy(stdout)
        )
    })?;
    Ok(document.findings)
}

/// The shape a passthrough binary writes to stdout. Mirrors the runtime's findings
/// output: a single object with a `findings` array.
#[derive(Debug, Deserialize)]
struct PassthroughDocument {
    #[serde(default)]
    findings: Vec<Finding>,
}

/// The `json` strategy: a [`Selector`] locates the issue rows; a [`FindingTemplate`]
/// projects each row into a [`Finding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonTransform {
    pub selector: Selector,
    pub finding: FindingTemplate,
}

impl JsonTransform {
    fn apply(
        &self,
        stdout: &[u8],
        exit_code: Option<i32>,
        input_file: Option<&str>,
        needs_invocations: Option<&BTreeMap<String, String>>,
    ) -> Result<Vec<Finding>> {
        let root: Value = serde_json::from_slice(stdout).with_context(|| {
            format!(
                "declarative json transform could not parse tool stdout as JSON; raw stdout: {:?}",
                String::from_utf8_lossy(stdout)
            )
        })?;
        let context = RenderContext {
            input_file,
            exit_code,
            needs_invocations,
        };
        let mut findings = Vec::new();
        for item in self.selector.select(&root)? {
            findings.push(self.finding.render(&item, context)?);
        }
        Ok(findings)
    }
}

/// The severity field of a [`FindingTemplate`]: either a fixed value known at
/// parse time, or a template that is rendered per row and parsed as a severity
/// string. The latter supports checks like `lint/js` where ESLint's JSON output
/// carries the per-finding severity (2 = error, 1 = warning) so both levels
/// can come from a single invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeverityTemplate {
    Fixed(Severity),
    Dynamic(Template),
}

/// A declared finding map. Each field is a [`Template`] (literal text + refs);
/// `severity` is either a fixed literal or a template rendered per row.
/// `line`/`column` are optional, so a finding may be line-less — the buildifier
/// format pass emits exactly that (the file isn't clean, but there is no single
/// offending line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingTemplate {
    pub path: Template,
    pub line: Option<Template>,
    pub column: Option<Template>,
    pub message: Template,
    pub severity: SeverityTemplate,
    pub remediations: Vec<Template>,
}

impl FindingTemplate {
    fn render(&self, item: &Value, context: RenderContext<'_>) -> Result<Finding> {
        let path = PathBuf::from(self.path.render(item, context)?);
        let line = self.render_number(self.line.as_ref(), "line", item, context)?;
        let column = self.render_number(self.column.as_ref(), "column", item, context)?;
        let message = self.message.render(item, context)?;
        let severity = match &self.severity {
            SeverityTemplate::Fixed(sev) => *sev,
            SeverityTemplate::Dynamic(tmpl) => {
                let rendered = tmpl.render(item, context)?;
                Severity::parse_with_default(Some(&rendered), Severity::Warning)
            }
        };
        let remediations = self
            .remediations
            .iter()
            .map(|remediation| remediation.render(item, context))
            .collect::<Result<Vec<_>>>()?;
        Ok(Finding {
            severity,
            message,
            location: Some(Location { path, line, column }),
            remediations,
            suggested_fix: None,
        })
    }

    fn render_number(
        &self,
        template: Option<&Template>,
        field: &str,
        item: &Value,
        context: RenderContext<'_>,
    ) -> Result<Option<u32>> {
        let Some(template) = template else {
            return Ok(None);
        };
        let rendered = template.render(item, context)?;
        let parsed = rendered
            .parse::<u32>()
            .with_context(|| format!("finding.{field} rendered to `{rendered}`, which is not a line/column number"))?;
        Ok(Some(parsed))
    }
}

/// The `linelist` strategy: the binary prints one file path per line on stdout for
/// files that have violations. Each non-empty line becomes one file-level finding.
///
/// If the tool exits non-zero but stdout is empty, the transform returns an error
/// rather than "clean". This distinguishes formatting violations (file paths printed)
/// from operational errors like parse failures (exit 1, no output).
///
/// `message` and `remediations` are templates; the only useful ref in this context
/// is `{{input.file}}` (the file passed to the per-file invocation). Unknown refs
/// are rejected at check-load time by the template parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineListTransform {
    pub message: Template,
    pub remediations: Vec<Template>,
    pub severity: Severity,
}

impl LineListTransform {
    fn apply(
        &self,
        stdout: &[u8],
        exit_code: Option<i32>,
        _input_file: Option<&str>,
        needs_invocations: Option<&BTreeMap<String, String>>,
    ) -> Result<Vec<Finding>> {
        let text = std::str::from_utf8(stdout).context("linelist transform: stdout is not valid UTF-8")?;
        let paths: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if paths.is_empty() && exit_code != Some(0) {
            // Non-zero exit with no file output is an operational error (e.g. a parse
            // failure), not a "no violations" result.
            let exit_desc = match exit_code {
                Some(code) => format!("returned exit code {code}"),
                None => "terminated without an exit code".to_string(),
            };
            bail!(
                "linelist transform: tool {exit_desc} and no output; this indicates an \
                 operational error (e.g. parse failure), not a clean result"
            );
        }
        // Render message and remediations per-path, using each stdout path as
        // `input.file`. This lets `{{input.file}}` work in both per_file and batch
        // mode: in per_file mode the stdout path is the file being checked; in batch
        // mode it is the specific unformatted file for that finding.
        paths
            .into_iter()
            .map(|path| {
                let context = RenderContext {
                    input_file: Some(path),
                    exit_code,
                    needs_invocations,
                };
                let message = self.message.render(NO_JSON_ITEM, context)?;
                let remediations = self
                    .remediations
                    .iter()
                    .map(|r| r.render(NO_JSON_ITEM, context))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Finding {
                    severity: self.severity,
                    message,
                    location: Some(Location {
                        path: PathBuf::from(path),
                        line: None,
                        column: None,
                    }),
                    remediations,
                    suggested_fix: None,
                })
            })
            .collect()
    }
}
