//! PreToolUse editorial enforcement for `gh pr|issue {create,edit,comment,review}`.
//!
//! This is enforcement Point 3 of the editorial-controls design
//! (`tools/boss/docs/designs/editorial-controls-for-agent-authored-prs-and-github-comments.md`):
//! a deterministic last-line-of-defence that runs the worker's proposed
//! PR/issue body and title through [`boss_editorial::evaluate`] *before*
//! the `gh` call reaches GitHub. It either allows the call, allows it
//! with a redacted body substituted in place, or denies it with
//! actionable feedback the worker can act on within the same turn.
//!
//! The module is deliberately self-contained and side-effect-light:
//! [`evaluate_gh_pretooluse`] is a pure-ish function (its only side
//! effect is overwriting a worker-owned `--body-file`, by design — R14)
//! returning an [`EditorialOutcome`]. The caller turns the outcome's
//! [`PreToolUseDecision`] into claude's hook-response JSON via
//! [`PreToolUseDecision::to_hook_output`] and emits the optional
//! [`EditorialAttention`] as a `WorkAttentionItem`.
//!
//! ## Handler steps (design Point 3)
//!
//! 1. Parse `--body` / `--body-file` / `--title` / `--message`; if
//!    `--editor` or `--web` is present, allow (there is no inspectable
//!    body — the worker is delegating to an editor or the browser).
//! 2. Run the body and title through the redactor; apply `Rewrite`
//!    redactions in place and collect `Block` hits.
//! 3. When `template_policy == Enforce` and the call is `gh pr
//!    create`/`edit`, compare the body's headings against the template's
//!    required headings and collect any missing sections.
//! 4. Decide: no findings → allow; all findings auto-rewritable and the
//!    body/title changed → allow with the mutated command (or the
//!    overwritten `--body-file`); any `Block`/structural finding → deny
//!    with a numbered, actionable reason.
//! 5. Loop guard (R3): a third deny of the same invocation within one
//!    execution flips to allow and emits an [`EditorialAttention`], so a
//!    worker that cannot make a body compliant ships it (flagged) rather
//!    than oscillating forever.
//!
//! ## Fail-open
//!
//! Anything the handler cannot inspect (a `--body-file` it cannot read,
//! a command it cannot classify, a call with neither body nor title)
//! returns [`PreToolUseDecision::Allow`]. The editorial controls are
//! advisory in a partition, never a hard block on the worker's progress
//! (R-unreachable).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

use boss_editorial::{CompiledRules, EditorialDecision, Finding, FindingKind};

use crate::gh_invocation::{self, GhNoun};

/// A third deny of the same invocation within one execution flips to
/// allow (R3). Calls 1 and 2 are denied; the 3rd is allowed with an
/// attention item so the worker cannot loop forever on a body it cannot
/// make compliant.
const DENY_LIMIT: u32 = 3;

// ---------------------------------------------------------------------------
// Public decision + outcome types
// ---------------------------------------------------------------------------

/// The PreToolUse permission decision the handler produces. Serialise to
/// claude's hook-response JSON with [`Self::to_hook_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolUseDecision {
    /// Allow the `gh` call unchanged.
    Allow,
    /// Allow the call, but with editorial redactions applied. When
    /// `updated_command` is `Some`, the worker's Bash command is replaced
    /// (the `--body` / `--title` value substituted). When it is `None`
    /// the redaction landed in a `--body-file` that was overwritten on
    /// disk, so the command itself is unchanged.
    AllowWithRewrite {
        updated_command: Option<String>,
        reason: String,
    },
    /// Deny the call. The worker reads `reason`, fixes the body, retries.
    Deny { reason: String },
}

impl PreToolUseDecision {
    /// Render this decision as claude's PreToolUse hook-output JSON.
    ///
    /// `tool_input` is the original Bash `tool_input` object; for an
    /// `AllowWithRewrite { updated_command: Some(cmd) }` the returned
    /// `updatedInput` is a clone of it with `command` replaced.
    pub fn to_hook_output(&self, tool_input: &Value) -> Value {
        match self {
            PreToolUseDecision::Allow => json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                }
            }),
            PreToolUseDecision::AllowWithRewrite {
                updated_command,
                reason,
            } => {
                let mut out = json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "permissionDecisionReason": reason,
                    }
                });
                if let Some(cmd) = updated_command {
                    let mut updated_input = tool_input.clone();
                    if let Some(obj) = updated_input.as_object_mut() {
                        obj.insert("command".to_owned(), Value::String(cmd.clone()));
                    }
                    out["hookSpecificOutput"]["updatedInput"] = updated_input;
                }
                out
            }
            PreToolUseDecision::Deny { reason } => json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            }),
        }
    }
}

/// What the hook did, for the `editorial_actions` audit log. Matches the
/// `action` vocabulary on `boss_protocol::EditorialAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorialActionKind {
    /// Allowed unchanged, or allowed-through after the loop guard fired.
    Allow,
    /// Body/title rewritten in place.
    Rewrite,
    /// Invocation denied.
    Deny,
}

impl EditorialActionKind {
    /// The audit-log string (`boss_protocol::EditorialAction::action`).
    pub fn as_str(self) -> &'static str {
        match self {
            EditorialActionKind::Allow => "allow",
            EditorialActionKind::Rewrite => "rewrite",
            EditorialActionKind::Deny => "deny",
        }
    }
}

/// A flagged review item the caller should surface as a
/// `WorkAttentionItem` (emitted only when the loop guard flips a repeated
/// deny to allow — R3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorialAttention {
    pub summary: String,
    pub detail: String,
}

/// The full result of evaluating one `gh` invocation against the
/// editorial rules.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorialOutcome {
    pub decision: PreToolUseDecision,
    /// The findings that drove the decision (empty for a clean allow).
    pub findings: Vec<Finding>,
    /// What the hook did, for the audit log.
    pub action: EditorialActionKind,
    /// Present only when the loop guard flipped a deny to an allow.
    pub attention: Option<EditorialAttention>,
}

impl EditorialOutcome {
    fn allow() -> Self {
        Self {
            decision: PreToolUseDecision::Allow,
            findings: Vec::new(),
            action: EditorialActionKind::Allow,
            attention: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Deny tracker (loop guard, R3)
// ---------------------------------------------------------------------------

/// In-memory counter of how many times each `(execution, command)` pair
/// has been denied, so the loop guard can flip the third deny to an
/// allow. State is per-engine-process and never persisted: a restart
/// resets the counters, which is the safe direction (worst case the
/// worker gets three fresh denies, never an indefinite block).
#[derive(Debug, Default)]
pub struct DenyTracker {
    inner: Mutex<HashMap<String, u32>>,
}

impl DenyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one deny-worthy evaluation of `(execution_id, command)` and
    /// return the cumulative count *including* this one. `1` and `2` mean
    /// "deny"; `DENY_LIMIT` (3) and above mean "flip to allow".
    fn record(&self, execution_id: &str, command: &str) -> u32 {
        let key = Self::key(execution_id, command);
        let mut guard = self.inner.lock().expect("DenyTracker mutex poisoned");
        let counter = guard.entry(key).or_insert(0);
        *counter += 1;
        *counter
    }

    /// Drop the counter for `(execution_id, command)`. Idempotent. Called
    /// once a clean allow / rewrite lands so a later identical-but-now-fixed
    /// command starts fresh.
    pub fn forget(&self, execution_id: &str, command: &str) {
        let key = Self::key(execution_id, command);
        self.inner
            .lock()
            .expect("DenyTracker mutex poisoned")
            .remove(&key);
    }

    fn key(execution_id: &str, command: &str) -> String {
        // `\u{1}` (SOH) cannot appear in a shell command, so it's a safe
        // separator that can't be forged by the command text.
        format!("{execution_id}\u{1}{command}")
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Editorial subcommands the hook enforces. `gh pr|issue` calls with any
/// other subcommand (`view`, `list`, `merge`, …) carry no
/// human-authored body and are allowed straight through.
fn is_enforced_subcommand(sub: &str) -> bool {
    matches!(sub, "create" | "edit" | "comment" | "review")
}

/// Evaluate one `gh pr|issue` or `cube pr ensure` Bash command against
/// the editorial rules.
///
/// - `command` — the worker's Bash command string (`tool_input.command`).
/// - `cwd` — the worker's working directory, used to resolve a relative
///   `--body-file` path (R14).
/// - `rules` — the product's compiled editorial rules (baked-in defaults
///   always apply on top).
/// - `template_body` — the repo's `PULL_REQUEST_TEMPLATE.md` text, or
///   `None`. Used only for `template_policy == Enforce` on `pr
///   create`/`edit` (and always for `cube pr ensure`).
/// - `execution_id` / `deny_tracker` — the loop-guard state (R3).
///
/// See the module docs for the full step list. Fails open on anything it
/// cannot inspect.
///
/// ## `cube pr ensure` coverage
///
/// Workers are instructed to create PRs via `cube pr ensure` rather than
/// calling `gh pr create` directly. `cube pr ensure` shells out to `gh pr
/// create` internally, making that call invisible to the PreToolUse hook.
/// This function intercepts the outer `cube pr ensure` command directly
/// and applies the same checks as for `gh pr create` (including template
/// enforcement).
///
/// ## Feature-flag gating
///
/// The **caller** is responsible for checking the `editorial_controls`
/// feature flag before invoking this function. When the flag is disabled
/// the call site should return `EditorialOutcome::allow()` directly and
/// skip the call entirely — that is the single choke-point gate for the
/// PreToolUse surface. The function itself does not check the flag so
/// that callers can unit-test it independently of the flag state.
pub fn evaluate_gh_pretooluse(
    command: &str,
    cwd: &Path,
    rules: &CompiledRules,
    template_body: Option<&str>,
    execution_id: &str,
    deny_tracker: &DenyTracker,
) -> EditorialOutcome {
    // Step 0: classify the command. `cube pr ensure` is treated as
    // equivalent to `gh pr create` — same template gating, same arg
    // parsing (both accept --body / --body-file / --title).
    let apply_template = if gh_invocation::is_cube_pr_ensure(command) {
        true
    } else {
        let Some(inv) = gh_invocation::classify(command) else {
            return EditorialOutcome::allow();
        };
        if !is_enforced_subcommand(&inv.subcommand) {
            return EditorialOutcome::allow();
        }
        inv.noun == GhNoun::Pr && matches!(inv.subcommand.as_str(), "create" | "edit")
    };

    // Step 1: parse the editorial-relevant args.
    let args = parse_gh_args(command);
    if args.editor || args.web {
        // The worker is delegating to $EDITOR / the browser — there is no
        // body the hook can see. Allow.
        return EditorialOutcome::allow();
    }

    // Resolve the body text + where it came from (for write-back).
    let (body_text, body_source) = match resolve_body(&args, cwd) {
        Some(pair) => pair,
        // A `--body-file` we couldn't read: fail open. The gh call will
        // fail on its own if the path is genuinely bad.
        None => return EditorialOutcome::allow(),
    };
    let title_text = args.title.as_ref().map(|s| s.value.as_str()).unwrap_or("");

    if body_text.is_empty() && title_text.is_empty() {
        // Nothing to evaluate (e.g. `gh pr edit --add-label`).
        return EditorialOutcome::allow();
    }

    // Step 3 gating: the template check only applies to PR create/edit
    // (and to cube pr ensure, which always creates a PR).
    let template_for_call = if apply_template { template_body } else { None };

    // Step 2 + 3: run the evaluator.
    let decision = boss_editorial::evaluate(&body_text, title_text, rules, template_for_call);

    match decision {
        EditorialDecision::Allow => {
            // Clean: clear any stale deny count for this command.
            deny_tracker.forget(execution_id, command);
            EditorialOutcome::allow()
        }
        EditorialDecision::Rewrite {
            body,
            title,
            findings,
        } => {
            // Step 4 (rewrite branch): apply the redactions in place.
            deny_tracker.forget(execution_id, command);
            apply_rewrite(command, &args, &body_source, &body, title.as_deref(), findings)
        }
        EditorialDecision::Block { findings } => {
            // Step 4 (deny branch) + step 5 (loop guard).
            let count = deny_tracker.record(execution_id, command);
            if count >= DENY_LIMIT {
                let reason = numbered_reason(&findings);
                EditorialOutcome {
                    decision: PreToolUseDecision::Allow,
                    findings,
                    action: EditorialActionKind::Allow,
                    attention: Some(EditorialAttention {
                        summary: "Editorial hook allowed a non-compliant GitHub call".to_owned(),
                        detail: format!(
                            "After {DENY_LIMIT} attempts the worker could not make this \
                             `gh` call compliant, so the editorial hook allowed it through \
                             to avoid blocking the run. Review the published text. \
                             Unresolved: {reason}"
                        ),
                    }),
                }
            } else {
                EditorialOutcome {
                    decision: PreToolUseDecision::Deny {
                        reason: numbered_reason(&findings),
                    },
                    findings,
                    action: EditorialActionKind::Deny,
                    attention: None,
                }
            }
        }
    }
}

/// Apply a `Rewrite` decision: overwrite a `--body-file` on disk and/or
/// substitute the `--body` / `--title` value in the command string.
fn apply_rewrite(
    command: &str,
    args: &GhArgs,
    body_source: &BodySource,
    new_body: &str,
    new_title: Option<&str>,
    findings: Vec<Finding>,
) -> EditorialOutcome {
    // Collect (span, replacement) edits to apply to the command string.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    match body_source {
        BodySource::Inline(span) => {
            edits.push((span.0, span.1, shell_quote(new_body)));
        }
        BodySource::File(path) => {
            // R14: overwrite the worker-owned body file at the resolved
            // path. Best-effort — if the write fails we still allow the
            // (now un-redacted) call rather than block the worker.
            if let Err(err) = std::fs::write(path, new_body) {
                tracing::warn!(
                    path = %path.display(),
                    ?err,
                    "editorial_hook: failed to overwrite --body-file; allowing original",
                );
            }
        }
        BodySource::None => {}
    }

    if let (Some(new_title), Some(title)) = (new_title, args.title.as_ref()) {
        edits.push((title.span.0, title.span.1, shell_quote(new_title)));
    }

    let reason = rewrite_reason(&findings);

    // Apply edits to the command string (descending by start so byte
    // offsets stay valid). `BodySource::File` contributes no command edit.
    let updated_command = if edits.is_empty() {
        None
    } else {
        edits.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out = command.to_owned();
        for (start, end, replacement) in edits {
            out.replace_range(start..end, &replacement);
        }
        Some(out)
    };

    EditorialOutcome {
        decision: PreToolUseDecision::AllowWithRewrite {
            updated_command,
            reason,
        },
        findings,
        action: EditorialActionKind::Rewrite,
        attention: None,
    }
}

/// Resolve the body text the worker proposed and remember where it came
/// from so a rewrite can land in the right place. Returns `None` only
/// when a `--body-file` is named but cannot be read (fail open).
fn resolve_body(args: &GhArgs, cwd: &Path) -> Option<(String, BodySource)> {
    if let Some(body) = &args.body {
        return Some((body.value.clone(), BodySource::Inline(body.span)));
    }
    if let Some(raw_path) = &args.body_file {
        let path = resolve_path(cwd, raw_path);
        match std::fs::read_to_string(&path) {
            Ok(text) => return Some((text, BodySource::File(path))),
            Err(_) => return None,
        }
    }
    if let Some(message) = &args.message {
        return Some((message.value.clone(), BodySource::Inline(message.span)));
    }
    Some((String::new(), BodySource::None))
}

/// Resolve `raw_path` against the worker's cwd. Absolute paths are used
/// verbatim; relative paths are joined onto `cwd` (R14).
fn resolve_path(cwd: &Path, raw_path: &str) -> PathBuf {
    let p = Path::new(raw_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Where the evaluated body came from, so a rewrite knows whether to edit
/// the command string (inline) or overwrite a file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodySource {
    /// `--body`/`--message` value at this `(start, end)` byte span in the
    /// command string.
    Inline((usize, usize)),
    /// `--body-file` at this resolved path.
    File(PathBuf),
    /// No body argument at all.
    None,
}

// ---------------------------------------------------------------------------
// Reason builders
// ---------------------------------------------------------------------------

/// A numbered, actionable deny reason from `Block` / `Template` findings.
fn numbered_reason(findings: &[Finding]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (i, f) in findings
        .iter()
        .filter(|f| matches!(f.kind, FindingKind::Block | FindingKind::Template))
        .enumerate()
    {
        let action = match f.kind {
            FindingKind::Template => f.description.clone(),
            _ => format!("rephrase to avoid {}", f.description),
        };
        parts.push(format!("{}) {}", i + 1, action));
    }
    format!(
        "This `gh` call violates editorial rules: {}. Please fix the body/title and retry \
         (write the corrected text to a file and pass it with --body-file).",
        parts.join("; ")
    )
}

/// A one-line summary of what a rewrite changed, for `decisionReason`.
fn rewrite_reason(findings: &[Finding]) -> String {
    let names: Vec<&str> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::Redact)
        .map(|f| f.description.as_str())
        .collect();
    format!("Editorial hook redacted: {}", names.join(", "))
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// A flag value plus its byte span in the original command, so a rewrite
/// can replace exactly that span and leave everything else verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpannedValue {
    value: String,
    /// `(start, end)` byte offsets of the *value* portion in the command
    /// (for `--flag=value` this excludes `--flag=`; for `--flag value` it
    /// is the whole value token including any surrounding quotes).
    span: (usize, usize),
}

/// The editorial-relevant arguments parsed out of a `gh pr|issue`
/// command.
#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
struct GhArgs {
    title: Option<SpannedValue>,
    body: Option<SpannedValue>,
    body_file: Option<String>,
    message: Option<SpannedValue>,
    editor: bool,
    web: bool,
}

/// Parse the editorial-relevant flags out of a `gh` command.
///
/// Recognises long and short forms: `--body`/`-b`, `--body-file`/`-F`,
/// `--title`/`-t`, `--message`/`-m`, and the bare `--editor` / `--web`.
/// Both `--flag value` and `--flag=value` shapes are handled. Quoting is
/// resolved (so `--body "a b"` yields the value `a b`) while the recorded
/// span still covers the original, quotes included, so a rewrite can
/// re-quote cleanly.
fn parse_gh_args(command: &str) -> GhArgs {
    let tokens = tokenize(command);
    let mut args = GhArgs::default();

    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        let raw = &tok.value;

        // `--flag=value` forms.
        if let Some((flag, _value)) = raw.split_once('=') {
            let value_start = tok.start + flag.len() + 1; // skip `flag=`
            let spanned = || SpannedValue {
                value: dequote(&command[value_start..tok.end]),
                span: (value_start, tok.end),
            };
            match flag {
                "--body" | "-b" => {
                    args.body = Some(spanned());
                    i += 1;
                    continue;
                }
                "--body-file" | "-F" => {
                    args.body_file = Some(dequote(&command[value_start..tok.end]));
                    i += 1;
                    continue;
                }
                "--title" | "-t" => {
                    args.title = Some(spanned());
                    i += 1;
                    continue;
                }
                "--message" | "-m" => {
                    args.message = Some(spanned());
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }

        // Bare boolean flags.
        match raw.as_str() {
            "--editor" => {
                args.editor = true;
                i += 1;
                continue;
            }
            "--web" | "-w" => {
                args.web = true;
                i += 1;
                continue;
            }
            _ => {}
        }

        // `--flag value` forms (value is the next token).
        let takes_value = matches!(
            raw.as_str(),
            "--body" | "-b" | "--body-file" | "-F" | "--title" | "-t" | "--message" | "-m"
        );
        if takes_value {
            if let Some(next) = tokens.get(i + 1) {
                let spanned = SpannedValue {
                    value: next.value.clone(),
                    span: (next.start, next.end),
                };
                match raw.as_str() {
                    "--body" | "-b" => args.body = Some(spanned),
                    "--body-file" | "-F" => args.body_file = Some(next.value.clone()),
                    "--title" | "-t" => args.title = Some(spanned),
                    "--message" | "-m" => args.message = Some(spanned),
                    _ => {}
                }
                i += 2;
                continue;
            }
        }

        i += 1;
    }

    args
}

/// A shell token with its unquoted value and its byte span (covering any
/// surrounding quotes) in the original command.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    value: String,
    start: usize,
    end: usize,
}

/// A small POSIX-ish tokenizer that records byte spans.
///
/// Handles whitespace separation, single quotes (literal), double quotes
/// (with backslash escapes for `"`, `\`, `$`, and `` ` ``), and
/// backslash escapes outside quotes. It is intentionally not a full
/// shell parser — it covers the shapes `gh pr|issue` commands actually
/// take, which is enough to extract flag values precisely.
///
/// Works at the byte level but is UTF-8 safe: only ASCII whitespace
/// separates tokens (so a multibyte char's continuation bytes are never
/// mistaken for a separator), and the accumulated bytes — which copy
/// whole multibyte sequences verbatim, dropping only ASCII quote /
/// backslash delimiters — re-form valid UTF-8.
fn tokenize(command: &str) -> Vec<Token> {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < len {
        // Skip whitespace between tokens (ASCII only — see fn docs).
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        let start = i;
        let mut buf: Vec<u8> = Vec::new();

        while i < len && !bytes[i].is_ascii_whitespace() {
            match bytes[i] {
                b'\'' => {
                    i += 1;
                    while i < len && bytes[i] != b'\'' {
                        buf.push(bytes[i]);
                        i += 1;
                    }
                    if i < len {
                        i += 1; // closing quote
                    }
                }
                b'"' => {
                    i += 1;
                    while i < len && bytes[i] != b'"' {
                        if bytes[i] == b'\\'
                            && i + 1 < len
                            && matches!(bytes[i + 1], b'"' | b'\\' | b'$' | b'`')
                        {
                            buf.push(bytes[i + 1]);
                            i += 2;
                        } else {
                            buf.push(bytes[i]);
                            i += 1;
                        }
                    }
                    if i < len {
                        i += 1; // closing quote
                    }
                }
                b'\\' if i + 1 < len => {
                    buf.push(bytes[i + 1]);
                    i += 2;
                }
                _ => {
                    buf.push(bytes[i]);
                    i += 1;
                }
            }
        }

        tokens.push(Token {
            value: String::from_utf8(buf).unwrap_or_default(),
            start,
            end: i,
        });
    }

    tokens
}

/// Unquote a raw command slice (the same logic the tokenizer applies),
/// used for `--flag=value` value extraction where the value is embedded
/// in the flag token.
fn dequote(raw: &str) -> String {
    // Reuse the tokenizer over a single-token slice. The value joins all
    // adjacent quoted/unquoted runs, which is exactly the shell behaviour
    // for `--body="a"'b'`.
    tokenize(raw)
        .into_iter()
        .map(|t| t.value)
        .collect::<Vec<_>>()
        .join("")
}

/// Single-quote `s` for safe re-insertion into a shell command, escaping
/// embedded single quotes as `'\''`. Deterministic and total.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use boss_protocol::{EditorialRules, RedactionKind, RedactionRule, TemplatePolicy};
    use tempfile::TempDir;

    use super::*;

    fn rules_default() -> CompiledRules {
        CompiledRules::compile(EditorialRules::default()).unwrap()
    }

    fn run(command: &str, cwd: &Path, tracker: &DenyTracker) -> EditorialOutcome {
        evaluate_gh_pretooluse(command, cwd, &rules_default(), None, "exec_test", tracker)
    }

    // --- tokenizer / arg parsing -------------------------------------------

    #[test]
    fn tokenize_resolves_quotes_and_spans() {
        let cmd = "gh pr create --body \"a b\" --title 'x y'";
        let args = parse_gh_args(cmd);
        assert_eq!(args.body.as_ref().unwrap().value, "a b");
        assert_eq!(args.title.as_ref().unwrap().value, "x y");
        // The body span must cover the quoted token so a rewrite replaces
        // the quotes too.
        let (s, e) = args.body.unwrap().span;
        assert_eq!(&cmd[s..e], "\"a b\"");
    }

    #[test]
    fn parse_equals_form() {
        let cmd = "gh pr create --body=hello --title=t";
        let args = parse_gh_args(cmd);
        assert_eq!(args.body.as_ref().unwrap().value, "hello");
        assert_eq!(args.title.as_ref().unwrap().value, "t");
        let (s, e) = args.body.unwrap().span;
        assert_eq!(&cmd[s..e], "hello");
    }

    #[test]
    fn parse_short_flags() {
        let cmd = "gh pr comment 3 -b 'hi there' -F notes.md";
        let args = parse_gh_args(cmd);
        assert_eq!(args.body.unwrap().value, "hi there");
        assert_eq!(args.body_file.unwrap(), "notes.md");
    }

    #[test]
    fn parse_editor_and_web_flags() {
        assert!(parse_gh_args("gh pr create --editor").editor);
        assert!(parse_gh_args("gh pr create --web").web);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    // --- decisions ---------------------------------------------------------

    #[test]
    fn clean_body_is_allowed() {
        let tmp = TempDir::new().unwrap();
        let out = run(
            "gh pr create --title 'Fix login' --body 'Aligns the button.'",
            tmp.path(),
            &DenyTracker::new(),
        );
        assert_eq!(out.decision, PreToolUseDecision::Allow);
        assert_eq!(out.action, EditorialActionKind::Allow);
    }

    #[test]
    fn editor_flag_short_circuits_to_allow() {
        let tmp = TempDir::new().unwrap();
        // Even with a dirty body, --editor means there's nothing to inspect.
        let out = run(
            "gh pr create --editor --body 'made by a Boss worker'",
            tmp.path(),
            &DenyTracker::new(),
        );
        assert_eq!(out.decision, PreToolUseDecision::Allow);
    }

    #[test]
    fn non_enforced_subcommand_is_allowed() {
        let tmp = TempDir::new().unwrap();
        let out = run("gh pr view 42", tmp.path(), &DenyTracker::new());
        assert_eq!(out.decision, PreToolUseDecision::Allow);
    }

    #[test]
    fn redactable_id_in_inline_body_rewrites_command() {
        let tmp = TempDir::new().unwrap();
        let cmd = "gh pr create --title t --body 'Fixes exec_18b07a506d2518d0_1b in prod.'";
        let out = run(cmd, tmp.path(), &DenyTracker::new());
        match &out.decision {
            PreToolUseDecision::AllowWithRewrite {
                updated_command: Some(new_cmd),
                ..
            } => {
                assert!(!new_cmd.contains("exec_18b07a506d2518d0_1b"), "id must be gone: {new_cmd}");
                assert!(new_cmd.starts_with("gh pr create --title t --body "));
            }
            other => panic!("expected AllowWithRewrite, got {other:?}"),
        }
        assert_eq!(out.action, EditorialActionKind::Rewrite);
    }

    #[test]
    fn blocking_phrase_in_body_denies() {
        let tmp = TempDir::new().unwrap();
        let out = run(
            "gh pr create --title t --body 'Created by a Boss worker.'",
            tmp.path(),
            &DenyTracker::new(),
        );
        match &out.decision {
            PreToolUseDecision::Deny { reason } => {
                assert!(reason.contains("Boss worker"), "reason: {reason}");
                assert!(reason.contains("1)"), "reason should be numbered: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
        assert_eq!(out.action, EditorialActionKind::Deny);
    }

    #[test]
    fn body_file_is_overwritten_in_place() {
        let tmp = TempDir::new().unwrap();
        let body_path = tmp.path().join("body.md");
        fs::write(&body_path, "Closes work on exec_18b07a506d2518d0_1b.\n").unwrap();

        let out = run(
            "gh pr create --title t --body-file body.md",
            tmp.path(),
            &DenyTracker::new(),
        );
        match &out.decision {
            PreToolUseDecision::AllowWithRewrite {
                updated_command, ..
            } => {
                // A --body-file rewrite leaves the command unchanged.
                assert!(updated_command.is_none(), "command should be unchanged");
            }
            other => panic!("expected AllowWithRewrite, got {other:?}"),
        }
        let rewritten = fs::read_to_string(&body_path).unwrap();
        assert!(!rewritten.contains("exec_18b07a506d2518d0_1b"), "file: {rewritten}");
    }

    #[test]
    fn unreadable_body_file_fails_open() {
        let tmp = TempDir::new().unwrap();
        let out = run(
            "gh pr create --title t --body-file does-not-exist.md",
            tmp.path(),
            &DenyTracker::new(),
        );
        assert_eq!(out.decision, PreToolUseDecision::Allow);
    }

    #[test]
    fn three_denies_flip_to_allow_with_attention() {
        let tmp = TempDir::new().unwrap();
        let tracker = DenyTracker::new();
        let cmd = "gh pr create --title t --body 'Created by a Boss worker.'";

        let first = run(cmd, tmp.path(), &tracker);
        assert!(matches!(first.decision, PreToolUseDecision::Deny { .. }));
        assert!(first.attention.is_none());

        let second = run(cmd, tmp.path(), &tracker);
        assert!(matches!(second.decision, PreToolUseDecision::Deny { .. }));
        assert!(second.attention.is_none());

        let third = run(cmd, tmp.path(), &tracker);
        assert_eq!(third.decision, PreToolUseDecision::Allow);
        assert_eq!(third.action, EditorialActionKind::Allow);
        let attention = third.attention.expect("third deny must emit an attention item");
        assert!(attention.detail.contains("Boss worker"), "detail: {}", attention.detail);
    }

    #[test]
    fn deny_count_is_per_command() {
        // Two different invocations don't share a deny budget.
        let tmp = TempDir::new().unwrap();
        let tracker = DenyTracker::new();
        let a = "gh pr create --title t --body 'the engine did it'";
        let b = "gh issue comment 1 --body 'the coordinator said so'";
        assert!(matches!(run(a, tmp.path(), &tracker).decision, PreToolUseDecision::Deny { .. }));
        assert!(matches!(run(b, tmp.path(), &tracker).decision, PreToolUseDecision::Deny { .. }));
        // `a` is still on its first deny, so a second `a` denies again
        // (not flipped).
        assert!(matches!(run(a, tmp.path(), &tracker).decision, PreToolUseDecision::Deny { .. }));
    }

    #[test]
    fn template_enforce_missing_section_denies_on_pr_create() {
        let tmp = TempDir::new().unwrap();
        let mut rules = EditorialRules::default();
        rules.template_policy = TemplatePolicy::Enforce;
        let compiled = CompiledRules::compile(rules).unwrap();
        let template = "## Summary\n\n## Test plan\n";
        let out = evaluate_gh_pretooluse(
            "gh pr create --title t --body '## Summary\n\nDid a thing.'",
            tmp.path(),
            &compiled,
            Some(template),
            "exec_test",
            &DenyTracker::new(),
        );
        match &out.decision {
            PreToolUseDecision::Deny { reason } => {
                assert!(reason.to_lowercase().contains("test plan"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn template_not_checked_on_pr_comment() {
        // template_policy applies only to pr create/edit; a comment with a
        // missing section is fine.
        let tmp = TempDir::new().unwrap();
        let mut rules = EditorialRules::default();
        rules.template_policy = TemplatePolicy::Enforce;
        let compiled = CompiledRules::compile(rules).unwrap();
        let template = "## Summary\n\n## Test plan\n";
        let out = evaluate_gh_pretooluse(
            "gh pr comment 5 --body 'looks good to me'",
            tmp.path(),
            &compiled,
            Some(template),
            "exec_test",
            &DenyTracker::new(),
        );
        assert_eq!(out.decision, PreToolUseDecision::Allow);
    }

    #[test]
    fn user_redaction_rewrites_inline_body() {
        let tmp = TempDir::new().unwrap();
        let compiled = CompiledRules::compile(EditorialRules {
            redactions: vec![RedactionRule {
                pattern: r"\bACME-\d+\b".to_owned(),
                replacement: "[ticket]".to_owned(),
                kind: RedactionKind::Rewrite,
            }],
            ..Default::default()
        })
        .unwrap();
        let cmd = "gh pr create --title t --body 'Closes ACME-1234 cleanly.'";
        let out = evaluate_gh_pretooluse(cmd, tmp.path(), &compiled, None, "exec_test", &DenyTracker::new());
        match out.decision {
            PreToolUseDecision::AllowWithRewrite {
                updated_command: Some(new_cmd),
                ..
            } => {
                assert!(new_cmd.contains("[ticket]"), "new: {new_cmd}");
                assert!(!new_cmd.contains("ACME-1234"));
            }
            other => panic!("expected AllowWithRewrite, got {other:?}"),
        }
    }

    #[test]
    fn title_redaction_rewrites_title_span() {
        let tmp = TempDir::new().unwrap();
        let cmd = "gh pr create --title 'close exec_18b07a506d2518d0_1b' --body 'clean body'";
        let out = run(cmd, tmp.path(), &DenyTracker::new());
        match out.decision {
            PreToolUseDecision::AllowWithRewrite {
                updated_command: Some(new_cmd),
                ..
            } => {
                assert!(!new_cmd.contains("exec_18b07a506d2518d0_1b"), "new: {new_cmd}");
                assert!(new_cmd.contains("--body 'clean body'"), "body untouched: {new_cmd}");
            }
            other => panic!("expected AllowWithRewrite, got {other:?}"),
        }
    }

    // --- hook-output JSON --------------------------------------------------

    #[test]
    fn allow_serializes_to_allow_decision() {
        let input = json!({ "command": "gh pr view" });
        let out = PreToolUseDecision::Allow.to_hook_output(&input);
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn rewrite_serializes_updated_input() {
        let input = json!({ "command": "gh pr create --body 'x'", "timeout": 5000 });
        let decision = PreToolUseDecision::AllowWithRewrite {
            updated_command: Some("gh pr create --body 'y'".to_owned()),
            reason: "Editorial hook redacted: exec_… identifier".to_owned(),
        };
        let out = decision.to_hook_output(&input);
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            "gh pr create --body 'y'"
        );
        // Other tool_input fields survive.
        assert_eq!(out["hookSpecificOutput"]["updatedInput"]["timeout"], 5000);
    }

    #[test]
    fn deny_serializes_to_deny_decision() {
        let input = json!({ "command": "gh pr create --body 'bad'" });
        let decision = PreToolUseDecision::Deny {
            reason: "nope".to_owned(),
        };
        let out = decision.to_hook_output(&input);
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], "nope");
    }
}
