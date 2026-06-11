use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::output::Severity;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodePatternsConfig {
    lang: String,
    #[serde(default)]
    rules: Vec<CodePatternRuleConfig>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodePatternRuleConfig {
    nocall: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PatternLanguage {
    Java,
}

#[derive(Debug)]
pub(super) struct CompiledCodePatternsConfig {
    pub(super) language: PatternLanguage,
    pub(super) rules: Vec<CompiledNoCallRule>,
}

#[derive(Debug)]
pub(super) struct CompiledNoCallRule {
    pub(super) pattern: CompiledNoCallPattern,
    pub(super) message: Option<String>,
    pub(super) remediation: Option<String>,
    pub(super) severity: Severity,
}

#[derive(Debug, Clone)]
pub(super) struct CompiledNoCallPattern {
    pub(super) receiver_type: String,
    pub(super) method_name: String,
    pub(super) arity: usize,
}

impl CompiledNoCallPattern {
    pub(super) fn render(&self) -> String {
        format!("{}#{}()", self.receiver_type, self.method_name)
    }
}

pub(super) fn parse_config(config: &toml::Value) -> Result<CompiledCodePatternsConfig> {
    let parsed: CodePatternsConfig = config
        .clone()
        .try_into()
        .context("invalid code-patterns check config")?;
    if parsed.rules.is_empty() {
        bail!("code-patterns check config must contain at least one `rules` entry");
    }

    let language = match parsed.lang.trim() {
        "java" => PatternLanguage::Java,
        "" => bail!("code-patterns check config must set non-empty `lang`"),
        other => bail!("unsupported code-patterns `lang`: {other}"),
    };

    let default_severity = Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error);
    let default_message = normalize_optional_string(parsed.message, "message")?;
    let default_remediation = normalize_optional_string(parsed.remediation, "remediation")?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for rule in parsed.rules {
        let pattern = parse_nocall_pattern(&rule.nocall)?;
        let message = normalize_optional_string(rule.message, "message")?.or_else(|| default_message.clone());
        let remediation =
            normalize_optional_string(rule.remediation, "remediation")?.or_else(|| default_remediation.clone());

        rules.push(CompiledNoCallRule {
            pattern,
            message,
            remediation,
            severity: Severity::parse_with_default(rule.severity.as_deref(), default_severity),
        });
    }

    Ok(CompiledCodePatternsConfig { language, rules })
}

fn normalize_optional_string(value: Option<String>, field_name: &str) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("code-patterns `{field_name}` must not be empty when present");
    }
    Ok(Some(trimmed.to_owned()))
}

fn parse_nocall_pattern(raw: &str) -> Result<CompiledNoCallPattern> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("code-patterns rule `nocall` must not be empty");
    }

    let (receiver_type, remainder) = raw
        .split_once('#')
        .context("code-patterns `nocall` must contain `#` between type and method")?;
    let receiver_type = receiver_type.trim();
    if receiver_type.is_empty() {
        bail!("code-patterns `nocall` must include a receiver type");
    }

    let open_paren = remainder
        .find('(')
        .context("code-patterns `nocall` must include a method signature")?;
    let close_paren = remainder
        .rfind(')')
        .context("code-patterns `nocall` must end with `)`")?;
    if close_paren != remainder.len() - 1 {
        bail!("code-patterns `nocall` must end with `)`");
    }
    let method_name = remainder[..open_paren].trim();
    if method_name.is_empty() {
        bail!("code-patterns `nocall` must include a method name");
    }

    let args = remainder[open_paren + 1..close_paren].trim();
    if !args.is_empty() {
        bail!("code-patterns `nocall` currently supports only zero-argument signatures `()`");
    }

    Ok(CompiledNoCallPattern {
        receiver_type: receiver_type.to_owned(),
        method_name: method_name.to_owned(),
        arity: 0,
    })
}
