//! Declarative transforms: the projection DSL that turns an invocation's
//! `(stdout, exit_code, file-it-ran-on)` into `Vec<Finding>`.
//!
//! Only the `json` strategy is implemented (enough for buildifier). The
//! [`Transform`] enum is the strategy slot: `regex` (parse stdout lines) and
//! `sarif` (map SARIF results) would each add a variant here and reuse the same
//! `apply(stdout, exit_code, input_file) → Vec<Finding>` signature. Computed
//! transforms that need real logic are where the wasm pure-function tier begins.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::output::{Finding, Location, Severity};

use super::selector::Selector;
use super::template::{RenderContext, Template};

/// A declared projection strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transform {
    Json(JsonTransform),
    // Future: Regex(RegexTransform), Sarif(SarifTransform).
}

impl Transform {
    /// Project an invocation's output into findings. `input_file` is the file the
    /// invocation ran on (per-file mode) or `None` (batch mode).
    pub fn apply(
        &self,
        stdout: &[u8],
        exit_code: Option<i32>,
        input_file: Option<&str>,
    ) -> Result<Vec<Finding>> {
        match self {
            Transform::Json(json) => json.apply(stdout, exit_code, input_file),
        }
    }
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
    ) -> Result<Vec<Finding>> {
        let root: Value = serde_json::from_slice(stdout).with_context(|| {
            format!(
                "declarative json transform could not parse tool stdout as JSON; raw stdout: {:?}",
                String::from_utf8_lossy(stdout)
            )
        })?;
        let context = RenderContext { input_file, exit_code };
        let mut findings = Vec::new();
        for item in self.selector.select(&root) {
            findings.push(self.finding.render(item, context)?);
        }
        Ok(findings)
    }
}

/// A declared finding map. Each field is a [`Template`] (literal text + refs);
/// `severity` is a literal. `line`/`column` are optional, so a finding may be
/// line-less — the buildifier format pass emits exactly that (the file isn't
/// clean, but there is no single offending line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingTemplate {
    pub path: Template,
    pub line: Option<Template>,
    pub column: Option<Template>,
    pub message: Template,
    pub severity: Severity,
    pub remediations: Vec<Template>,
}

impl FindingTemplate {
    fn render(&self, item: &Value, context: RenderContext<'_>) -> Result<Finding> {
        let path = PathBuf::from(self.path.render(item, context)?);
        let line = self.render_number(self.line.as_ref(), "line", item, context)?;
        let column = self.render_number(self.column.as_ref(), "column", item, context)?;
        let message = self.message.render(item, context)?;
        let remediations = self
            .remediations
            .iter()
            .map(|remediation| remediation.render(item, context))
            .collect::<Result<Vec<_>>>()?;
        Ok(Finding {
            severity: self.severity,
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
