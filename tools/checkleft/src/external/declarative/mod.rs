//! The **declarative** external-check runtime — one of the two external-check
//! runtimes (the other is `wasm`, for sandboxed pure computation).
//!
//! A declarative check ships *zero check-authored code*. The package manifest
//! fully describes how to run a wrapped tool, and the checkleft framework — not a
//! sandboxed guest — owns binary resolution and invocation. A declarative check
//! decomposes into: select files → resolve declared binaries → run declared
//! invocations → apply declared transforms → emit [`Finding`]s.
//!
//! This runtime is the single "framework runs declared invocations + transforms"
//! path. It **subsumes the former `exec-v1` tier**: an `exec` check (a custom
//! binary that emits checkleft findings JSON directly) is just an invocation with
//! the `passthrough` transform. buildifier is the other forcing example: file
//! selection, binary resolution, ordered invocations with explicit exit-code
//! semantics, and a JSON→Finding projection — but no real computation, so it
//! collapses to pure declaration.
//!
//! # Transform strategies
//!
//! `json` (selector + finding map) and `passthrough` (the binary already emits a
//! findings document) are implemented. `regex` and `sarif` are reserved as a
//! future strategy slot (see [`transform`]). Invocation sandboxing is explicitly
//! out of scope (deferred by design).
//!
//! [`Finding`]: crate::output::Finding

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::output::Severity;

pub mod executor;
pub mod resolve;
pub mod selector;
pub mod template;
pub mod transform;

#[cfg(test)]
mod tests;

/// Hermetic end-to-end parity test against a real (runfiles-staged) buildifier.
#[cfg(test)]
mod parity_e2e;

pub use executor::run_declarative_check;

use selector::Selector;
use template::Template;

// ── validated package model ────────────────────────────────────────────────────

/// A fully-resolved declarative check package: the framework reads this and needs
/// nothing else (no guest code) to run the wrapped tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckDeclarativePackage {
    /// Declared binary requirements ("named holes"), keyed by name. Each carries a
    /// default binding; a CHECKS-config override may substitute a different one.
    pub needs: BTreeMap<String, BinaryRequirement>,
    /// File globs the check applies to. The framework selects matching changed
    /// files before running any invocation.
    pub applies_to: Vec<String>,
    /// Ordered, self-contained invocation specs. Findings from all invocations
    /// concatenate in order.
    pub invocations: Vec<Invocation>,
}

/// A declared binary requirement with a default binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryRequirement {
    pub default: BinaryBinding,
}

/// How a declared binary is resolved to a concrete executable path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryBinding {
    /// A Bazel label, built + resolved to its executable (environment-conditional:
    /// requires a Bazel workspace). Mirrors the built-in buildifier's
    /// `buildifier_target` path.
    Bazel(String),
    /// A direct path or PATH name — the portable fallback (no Bazel involved).
    Path(String),
}

/// One self-contained invocation of a declared binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub id: String,
    /// Which declared binary (key into [`ExternalCheckDeclarativePackage::needs`]).
    pub run: String,
    pub mode: InvocationMode,
    /// Argument templates. `{{files}}` (batch) expands to the matched file list;
    /// `{{file}}` (per-file) is substituted with the single file.
    pub args: Vec<String>,
    pub exit: ExitSemantics,
    pub transform: transform::Transform,
}

/// Whether an invocation runs once over the whole matched batch or once per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationMode {
    Batch,
    PerFile,
}

/// Explicit exit-code → outcome mapping. Load-bearing: a tool that exits nonzero
/// because it *crashed* must surface as a check error, never silently as "clean".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitSemantics {
    /// Per-code outcomes (e.g. `0 → findings`).
    codes: BTreeMap<i32, ExitOutcome>,
    /// Fallback for any unlisted code.
    default: ExitOutcome,
}

impl ExitSemantics {
    /// Classify an exit code. A missing/None code (process killed by signal) is
    /// treated as the default outcome.
    pub fn classify(&self, code: Option<i32>) -> ExitOutcome {
        match code {
            Some(code) => self.codes.get(&code).copied().unwrap_or(self.default),
            None => self.default,
        }
    }
}

/// What an exit code means for a check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Clean: no findings, no error.
    Ok,
    /// Run the transform over stdout to produce findings.
    Findings,
    /// The tool failed — surface as a check error (never silent).
    Error,
}

// ── raw manifest (TOML) → validated model ──────────────────────────────────────

/// Raw declarative fields grouped for validation. The individual fields are
/// deserialized on `RawExternalCheckPackage` (in the parent module) and gathered
/// here; they are rejected in artifact/exec modes and required in declarative mode.
#[derive(Debug)]
pub(super) struct RawDeclarativeFields {
    pub applies_to: Vec<String>,
    pub needs: BTreeMap<String, RawBinaryRequirement>,
    pub invocations: Vec<RawInvocation>,
}

impl RawDeclarativeFields {
    /// True when none of the declarative-only fields are set — used by the parent
    /// module to reject them in artifact/exec modes.
    pub(super) fn is_empty(&self) -> bool {
        self.applies_to.is_empty() && self.needs.is_empty() && self.invocations.is_empty()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawBinaryRequirement {
    default: RawBinaryBinding,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinaryBinding {
    #[serde(default)]
    bazel: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawInvocation {
    id: String,
    run: String,
    mode: String,
    #[serde(default)]
    args: Vec<String>,
    exit: BTreeMap<String, String>,
    transform: RawTransform,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTransform {
    kind: String,
    #[serde(default)]
    select: Option<String>,
    #[serde(default)]
    finding: Option<RawFinding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFinding {
    path: String,
    #[serde(default)]
    line: Option<String>,
    #[serde(default)]
    column: Option<String>,
    message: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediations: Vec<String>,
}

/// Validate the raw declarative fields into the [`ExternalCheckDeclarativePackage`]
/// model. Called by the parent manifest validator for `mode = "declarative"`.
pub(super) fn validate_declarative_implementation(
    raw: RawDeclarativeFields,
) -> Result<ExternalCheckDeclarativePackage> {
    if raw.applies_to.is_empty() {
        bail!("declarative package must declare a non-empty `applies_to` glob list");
    }
    if raw.needs.is_empty() {
        bail!("declarative package must declare at least one binary in `needs`");
    }
    if raw.invocations.is_empty() {
        bail!("declarative package must declare at least one `invocations` entry");
    }

    let mut needs = BTreeMap::new();
    for (name, requirement) in raw.needs {
        needs.insert(name.clone(), validate_requirement(&name, requirement)?);
    }

    let mut invocations = Vec::with_capacity(raw.invocations.len());
    for raw_invocation in raw.invocations {
        invocations.push(validate_invocation(&needs, raw_invocation)?);
    }

    Ok(ExternalCheckDeclarativePackage {
        needs,
        applies_to: raw.applies_to,
        invocations,
    })
}

fn validate_requirement(name: &str, raw: RawBinaryRequirement) -> Result<BinaryRequirement> {
    let binding = match (raw.default.bazel, raw.default.path) {
        (Some(_), Some(_)) => bail!(
            "binary `{name}` default binding must set exactly one of `bazel` or `path`, not both"
        ),
        (Some(bazel), None) => BinaryBinding::Bazel(non_empty(&format!("needs.{name}.default.bazel"), bazel)?),
        (None, Some(path)) => BinaryBinding::Path(non_empty(&format!("needs.{name}.default.path"), path)?),
        (None, None) => bail!("binary `{name}` default binding must set one of `bazel` or `path`"),
    };
    Ok(BinaryRequirement { default: binding })
}

fn validate_invocation(
    needs: &BTreeMap<String, BinaryRequirement>,
    raw: RawInvocation,
) -> Result<Invocation> {
    let id = non_empty("invocations[].id", raw.id)?;
    let run = non_empty("invocations[].run", raw.run)?;
    if !needs.contains_key(&run) {
        bail!("invocation `{id}` references unknown binary `{run}` (not declared in `needs`)");
    }

    let mode = match raw.mode.as_str() {
        "batch" => InvocationMode::Batch,
        "per_file" => InvocationMode::PerFile,
        other => bail!("invocation `{id}` has invalid mode `{other}` (expected `batch` or `per_file`)"),
    };

    validate_args_for_mode(&id, mode, &raw.args)?;
    let exit = validate_exit(&id, raw.exit)?;
    let transform = validate_transform(&id, raw.transform)?;

    Ok(Invocation {
        id,
        run,
        mode,
        args: raw.args,
        exit,
        transform,
    })
}

fn validate_args_for_mode(id: &str, mode: InvocationMode, args: &[String]) -> Result<()> {
    let has_files = args.iter().any(|a| a == "{{files}}");
    let has_file = args.iter().any(|a| a.contains("{{file}}"));
    match mode {
        InvocationMode::Batch if !has_files => {
            bail!("batch invocation `{id}` must include a standalone `{{{{files}}}}` arg")
        }
        InvocationMode::PerFile if !has_file => {
            bail!("per_file invocation `{id}` must include `{{{{file}}}}` in its args")
        }
        _ => Ok(()),
    }
}

fn validate_exit(id: &str, raw: BTreeMap<String, String>) -> Result<ExitSemantics> {
    if raw.is_empty() {
        bail!("invocation `{id}` must declare `exit` semantics");
    }
    let mut codes = BTreeMap::new();
    let mut default = None;
    for (key, value) in raw {
        let outcome = parse_outcome(id, &value)?;
        if key == "default" {
            default = Some(outcome);
        } else {
            let code: i32 = key
                .parse()
                .map_err(|_| anyhow::anyhow!("invocation `{id}` exit key `{key}` is not an integer or `default`"))?;
            codes.insert(code, outcome);
        }
    }
    let Some(default) = default else {
        bail!("invocation `{id}` exit semantics must include a `default` outcome (so crashes surface as errors)");
    };
    Ok(ExitSemantics { codes, default })
}

fn parse_outcome(id: &str, raw: &str) -> Result<ExitOutcome> {
    match raw {
        "ok" => Ok(ExitOutcome::Ok),
        "findings" => Ok(ExitOutcome::Findings),
        "error" => Ok(ExitOutcome::Error),
        other => bail!("invocation `{id}` has invalid exit outcome `{other}` (expected `ok`, `findings`, or `error`)"),
    }
}

fn validate_transform(id: &str, raw: RawTransform) -> Result<transform::Transform> {
    match raw.kind.as_str() {
        "json" => {
            let select = raw
                .select
                .ok_or_else(|| anyhow::anyhow!("json transform for invocation `{id}` requires `select`"))?;
            let finding = raw
                .finding
                .ok_or_else(|| anyhow::anyhow!("json transform for invocation `{id}` requires `finding`"))?;
            let selector = Selector::parse(&select)
                .map_err(|err| anyhow::anyhow!("invocation `{id}` has invalid `select`: {err}"))?;
            let finding = validate_finding(id, finding)?;
            Ok(transform::Transform::Json(transform::JsonTransform { selector, finding }))
        }
        // The identity transform: the binary already emits checkleft findings JSON
        // directly. This is how an old `exec-v1` check expresses itself in the
        // unified declarative runtime — it ships no selector/finding map because
        // its stdout *is* the finding document.
        "passthrough" => {
            if raw.select.is_some() {
                bail!("passthrough transform for invocation `{id}` must not set `select`");
            }
            if raw.finding.is_some() {
                bail!("passthrough transform for invocation `{id}` must not set `finding`");
            }
            Ok(transform::Transform::Passthrough)
        }
        // Future strategy slot — intentionally unbuilt. A `regex` strategy would
        // parse stdout lines; a `sarif` strategy would map SARIF results. Both fit
        // the same `(stdout, exit_code, context) → Vec<Finding>` seam;
        // richer/computed transforms are where the wasm pure-function tier takes over.
        "regex" | "sarif" => bail!(
            "transform kind `{}` is reserved for a future spike; only `json` and `passthrough` are implemented",
            raw.kind
        ),
        other => bail!("invocation `{id}` has unknown transform kind `{other}`"),
    }
}

fn validate_finding(id: &str, raw: RawFinding) -> Result<transform::FindingTemplate> {
    let parse = |field: &str, value: String| -> Result<Template> {
        Template::parse(&value)
            .map_err(|err| anyhow::anyhow!("invocation `{id}` finding.{field} is invalid: {err}"))
    };
    Ok(transform::FindingTemplate {
        path: parse("path", raw.path)?,
        line: raw.line.map(|line| parse("line", line)).transpose()?,
        column: raw.column.map(|column| parse("column", column)).transpose()?,
        message: parse("message", raw.message)?,
        severity: Severity::parse_with_default(raw.severity.as_deref(), Severity::Warning),
        remediations: raw
            .remediations
            .into_iter()
            .map(|remediation| parse("remediations[]", remediation))
            .collect::<Result<Vec<_>>>()?,
    })
}

fn non_empty(field: &str, value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("field `{field}` must not be empty");
    }
    Ok(trimmed.to_owned())
}
