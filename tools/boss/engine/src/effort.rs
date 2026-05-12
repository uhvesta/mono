//! Effort-level → dispatch knobs (Claude effort value, default
//! model, prompt addendum) per the merged
//! [`tools/boss/docs/designs/effort-and-model-estimation.md`] (PR
//! #370) §Q2.
//!
//! The mapping table is centralised here on purpose: the design's
//! open question 2 explicitly says we expect to retune the per-level
//! effort/model defaults without a schema change, so every consumer
//! that needs an effort-derived knob looks it up through this module
//! rather than re-spelling the table.

use boss_protocol::EffortLevel;

/// Engine default Claude model slug used when neither
/// `tasks.model_override`, the row's effort-level default, nor the
/// parent product's `default_model` is set (design §Q3 step 4).
///
/// The dispatcher always passes a concrete `--model` slug — design
/// §Q2 says we surface the chosen model on the dispatch
/// instrumentation stream regardless of how it was resolved, and
/// that's only useful if the engine-default branch resolves to an
/// explicit slug rather than relying on `claude`'s implicit default.
/// Today this matches what `claude` itself would pick (Opus 4.7);
/// retune when that drifts.
pub const ENGINE_DEFAULT_MODEL: &str = "claude-opus-4-7";

/// What the row's effort level maps to for `claude --effort`. Note
/// these are **claude**'s vocabulary, not Boss's — `Trivial` becomes
/// `"low"` on the wire, etc. The numbers in this table follow
/// Claude's published per-model recommendations as captured in the
/// design's Q2 §"The chosen mapping" table.
pub fn claude_effort_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    }
}

/// Default model slug for a given effort level, used when the row
/// has no explicit `model_override` (design §Q3 step 2).
pub fn default_model_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial => "claude-haiku-4-5-20251001",
        EffortLevel::Small | EffortLevel::Medium => "claude-sonnet-4-6",
        EffortLevel::Large | EffortLevel::Max => "claude-opus-4-7",
    }
}

/// Optional per-level worker-prompt addendum, prepended to the
/// existing prompt body. `None` for levels where the existing
/// task-implementation framing is already the right framing — we
/// deliberately don't nudge a `trivial` worker into writing a plan
/// it doesn't need.
pub fn prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(
            "Begin with a written plan. Identify the files you expect to touch and the \
             order you'll touch them in. Confirm the approach against the work item's \
             description before writing code.",
        ),
    }
}

/// Resolved dispatch knobs for one worker spawn. The dispatcher
/// builds this once from the row's `effort_level` / `model_override`,
/// the parent product's `default_model`, and the engine default, then
/// uses it to construct the worker's `claude` invocation and the
/// prompt-addendum prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnConfig {
    /// The level on the row at dispatch time. `None` when the row is
    /// untagged (legacy rows, or rows the coordinator's heuristic
    /// hasn't run against yet). Carried through to the dispatch
    /// stream so an operator can tell whether a Sonnet-on-`high`
    /// spawn came from `effort_level = small` (deliberate) or from
    /// a fall-through.
    pub effort_level: Option<EffortLevel>,
    /// Claude effort value to pass to `claude --effort`. `None` when
    /// the row has no `effort_level` — per design §Q2 the dispatcher
    /// omits the flag entirely in that case and lets `claude` fall
    /// through to its own default of `high`.
    pub claude_effort: Option<&'static str>,
    /// Resolved model slug. Always present: the dispatcher passes
    /// `--model <slug>` even for the engine-default fall-through so
    /// the choice is visible on the dispatch stream.
    pub model: String,
    /// Per-level prompt addendum to prepend to `.claude/initial-prompt.txt`.
    /// `None` when the level has no addendum (or no level is set).
    pub prompt_addendum: Option<&'static str>,
}

impl SpawnConfig {
    /// Worker spawn line written into the libghostty pane via the
    /// spawn RPC's `initial_input`. `--model` is always present;
    /// `--effort` is present only when the row carries an effort
    /// level (per design §Q2: omit and let `claude` fall through to
    /// `high` for untagged rows).
    ///
    /// The trailing newline is what the pane treats as the user
    /// hitting return — match today's behaviour byte-for-byte.
    pub fn claude_invocation(&self) -> String {
        let mut cmd = format!("claude --model {}", self.model);
        if let Some(effort) = self.claude_effort {
            cmd.push_str(" --effort ");
            cmd.push_str(effort);
        }
        cmd.push_str(" \"$(cat .claude/initial-prompt.txt)\"\n");
        cmd
    }
}

/// Resolve dispatch knobs per design §Q3 precedence:
/// 1. `tasks.model_override` (when non-empty after trim).
/// 2. Effort-level default — only when the row has an `effort_level`.
/// 3. `products.default_model` (when non-empty after trim).
/// 4. [`ENGINE_DEFAULT_MODEL`].
///
/// The effort value and prompt addendum follow `effort_level` only;
/// `model_override` does not change them (design §Q3: "a user who
/// overrides to Haiku on a `medium` row is asking 'use Haiku for
/// this one,' not 'treat this as a trivial.'").
pub fn resolve_spawn_config(
    effort_level: Option<EffortLevel>,
    model_override: Option<&str>,
    product_default_model: Option<&str>,
) -> SpawnConfig {
    let model = if let Some(m) = model_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        m.to_owned()
    } else if let Some(level) = effort_level {
        default_model_for_level(level).to_owned()
    } else if let Some(pd) = product_default_model
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        pd.to_owned()
    } else {
        ENGINE_DEFAULT_MODEL.to_owned()
    };

    SpawnConfig {
        effort_level,
        claude_effort: effort_level.map(claude_effort_for_level),
        model,
        prompt_addendum: effort_level.and_then(prompt_addendum_for_level),
    }
}

#[cfg(test)]
mod tests {
    //! The cases below are the rows in the design's §Q3 precedence
    //! table — change them only when the design changes.

    use super::*;

    #[test]
    fn null_row_falls_through_to_engine_default() {
        let cfg = resolve_spawn_config(None, None, None);
        assert_eq!(cfg.effort_level, None);
        assert_eq!(cfg.claude_effort, None);
        assert_eq!(cfg.model, ENGINE_DEFAULT_MODEL);
        assert_eq!(cfg.prompt_addendum, None);
    }

    #[test]
    fn null_row_with_product_default_uses_product_default() {
        let cfg = resolve_spawn_config(None, None, Some("claude-sonnet-4-6"));
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert_eq!(cfg.claude_effort, None);
    }

    #[test]
    fn empty_product_default_does_not_satisfy_precedence_step_3() {
        let cfg = resolve_spawn_config(None, None, Some("   "));
        assert_eq!(cfg.model, ENGINE_DEFAULT_MODEL);
    }

    #[test]
    fn effort_level_alone_picks_level_default_model() {
        let trivial = resolve_spawn_config(Some(EffortLevel::Trivial), None, None);
        assert_eq!(trivial.model, "claude-haiku-4-5-20251001");
        assert_eq!(trivial.claude_effort, Some("low"));
        assert_eq!(trivial.prompt_addendum, None);

        let small = resolve_spawn_config(Some(EffortLevel::Small), None, None);
        assert_eq!(small.model, "claude-sonnet-4-6");
        assert_eq!(small.claude_effort, Some("medium"));
        assert_eq!(small.prompt_addendum, None);

        let medium = resolve_spawn_config(Some(EffortLevel::Medium), None, None);
        assert_eq!(medium.model, "claude-sonnet-4-6");
        assert_eq!(medium.claude_effort, Some("high"));
        assert!(
            medium.prompt_addendum.unwrap().starts_with("Sketch"),
            "medium addendum should be the 'sketch a plan' nudge",
        );

        let large = resolve_spawn_config(Some(EffortLevel::Large), None, None);
        assert_eq!(large.model, "claude-opus-4-7");
        assert_eq!(large.claude_effort, Some("xhigh"));
        assert!(large.prompt_addendum.unwrap().starts_with("Begin with"));

        let max = resolve_spawn_config(Some(EffortLevel::Max), None, None);
        assert_eq!(max.model, "claude-opus-4-7");
        assert_eq!(max.claude_effort, Some("max"));
        // large and max share the prompt addendum (design §Q2 table).
        assert_eq!(max.prompt_addendum, large.prompt_addendum);
    }

    #[test]
    fn model_override_beats_effort_default_but_keeps_effort_value_and_addendum() {
        // Row: effort = medium, model_override = opus. Design §Q3 says
        // the override changes the model only; effort + addendum still
        // follow `effort_level`.
        let cfg =
            resolve_spawn_config(Some(EffortLevel::Medium), Some("opus"), Some("claude-sonnet-4-6"));
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.claude_effort, Some("high"));
        assert!(cfg.prompt_addendum.unwrap().starts_with("Sketch"));
    }

    #[test]
    fn model_override_beats_product_default_when_effort_is_unset() {
        let cfg =
            resolve_spawn_config(None, Some("claude-haiku-4-5-20251001"), Some("claude-opus-4-7"));
        assert_eq!(cfg.model, "claude-haiku-4-5-20251001");
        assert_eq!(cfg.claude_effort, None);
        assert_eq!(cfg.prompt_addendum, None);
    }

    #[test]
    fn empty_model_override_falls_through() {
        // An empty/whitespace override is the same as "no override" —
        // the schema sibling task's `normalize_model_override` already
        // canonicalises empty → NULL on insert, but the dispatcher
        // tolerates the looser shape so a hand-edited DB row doesn't
        // produce `claude --model ""`.
        let cfg = resolve_spawn_config(Some(EffortLevel::Large), Some("   "), None);
        assert_eq!(cfg.model, "claude-opus-4-7");
    }

    #[test]
    fn null_row_invocation_matches_today_plus_explicit_model() {
        // Regression: the only thing that must change for an untagged
        // row is the explicit `--model` slug. No `--effort`, no other
        // flags, same trailing newline. The "today" baseline is the
        // string at `runner.rs:279` before this change.
        let cfg = resolve_spawn_config(None, None, None);
        assert_eq!(
            cfg.claude_invocation(),
            "claude --model claude-opus-4-7 \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn trivial_invocation_includes_both_flags() {
        let cfg = resolve_spawn_config(Some(EffortLevel::Trivial), None, None);
        assert_eq!(
            cfg.claude_invocation(),
            "claude --model claude-haiku-4-5-20251001 --effort low \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn medium_with_override_uses_override_model_and_medium_effort() {
        let cfg = resolve_spawn_config(Some(EffortLevel::Medium), Some("opus"), None);
        assert_eq!(
            cfg.claude_invocation(),
            "claude --model opus --effort high \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }
}
