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

use std::path::Path;

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
/// Using the `"opus"` family alias means this auto-tracks the latest
/// Opus snapshot without requiring a code change on each model release.
pub const ENGINE_DEFAULT_MODEL: &str = "opus";

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
///
/// Family aliases (`"haiku"`, `"sonnet"`, `"opus"`) are used so the
/// engine auto-tracks the latest snapshot per family without requiring
/// a code change on each model release. Direct-API summarization
/// (see [`crate::live_status::SUMMARY_MODEL`]) still uses a pinned
/// model — that path doesn't go through the worker CLI.
pub fn default_model_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial => "haiku",
        EffortLevel::Small | EffortLevel::Medium => "sonnet",
        EffortLevel::Large | EffortLevel::Max => "opus",
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
    /// Permission mode rules (corp env vs personal laptop observations,
    /// 2026-05-14):
    /// - Opus: always `--permission-mode auto`. Corp env does not default to
    ///   it, so it must be explicit.
    /// - Sonnet/Haiku: controlled by `non_opus_auto_mode` (the
    ///   `workers.non_opus_permission_mode` setting). `false` (default, personal
    ///   laptop) → `--dangerously-skip-permissions`. `true` (corp laptop, where
    ///   dangerously-skip is forbidden) → `--permission-mode auto`.
    ///
    /// `settings_path`, when `Some`, is added as `--settings <path>` so
    /// claude loads the engine's worker hooks/deny rules from a file
    /// *outside* the workspace (merged on top of the repo's own
    /// `.claude/settings.json`). See [`crate::worker_setup`] for why
    /// the settings never live in the workspace tree.
    ///
    /// The trailing newline is what the pane treats as the user
    /// hitting return — match today's behaviour byte-for-byte.
    pub fn claude_invocation(
        &self,
        non_opus_auto_mode: bool,
        settings_path: Option<&Path>,
    ) -> String {
        let mut cmd = format!("claude --model {}", self.model);
        if let Some(effort) = self.claude_effort {
            cmd.push_str(" --effort ");
            cmd.push_str(effort);
        }
        if model_is_opus(&self.model) {
            cmd.push_str(" --permission-mode auto");
        } else if non_opus_auto_mode {
            cmd.push_str(" --permission-mode auto");
        } else {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if let Some(settings) = settings_path {
            // Single-quote the path so a `$TMPDIR` with spaces survives
            // the pane's shell. Worker settings paths never contain a
            // single quote, so naive single-quoting is sufficient.
            cmd.push_str(&format!(" --settings '{}'", settings.display()));
        }
        cmd.push_str(" \"$(cat .claude/initial-prompt.txt)\"\n");
        cmd
    }
}

/// Returns `true` iff the resolved model slug belongs to the Opus family.
/// Matching is liberal and case-insensitive: any id that contains the
/// substring `"opus"` counts as Opus. Non-Opus models (Haiku, Sonnet, …)
/// return `false`.
pub fn model_is_opus(model: &str) -> bool {
    model.to_ascii_lowercase().contains("opus")
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

// ---------------------------------------------------------------------------
// Marker corpus + audit thresholds — design §Q4 + Q4 follow-up
// ---------------------------------------------------------------------------
//
// The marker tables below are the §Q4 rules' string-match
// vocabulary, lifted into code so the `boss product audit-effort`
// report (PR #370 follow-up) can compute per-marker match counts
// against the chore corpus without re-running the coordinator's
// LLM-driven heuristic. They mirror the design exactly:
//
// - `INVESTIGATE_MARKERS`        ← §Q4 rule 2 (→ `large`)
// - `MULTI_SUBSYSTEM_HINTS`      ← §Q4 rule 4 (→ `medium`)
// - `MECHANICAL_EDIT_MARKERS`    ← §Q4 rule 5 (→ `trivial`)
//
// The audit thresholds live in the same module on purpose: the
// dispatcher's effort table (above), the marker corpus, and the
// rates that flag those markers for retune are the same family of
// "knobs we expect to tune without a schema change" called out in
// design §Q2 / Q4. Keeping them in one file means a retune is one
// PR.

/// §Q4 rule 2 marker list — "title or description matches an
/// `investigate` family marker → `large`." Stored verbatim from the
/// design; the matcher is case-insensitive whole-word.
pub const INVESTIGATE_MARKERS: &[&str] = &[
    "investigate",
    "audit",
    "instrument",
    "diagnose",
    "end-to-end",
    "root cause",
    "architect",
    "redesign",
    "migrate",
    "rearchitect",
];

/// §Q4 rule 4 hint list — "title or description names a multi-file
/// or multi-subsystem hint → `medium`." Subsystem names are
/// the module-path vocabulary the design's "spans" / "across"
/// callouts shorthand for; the literal connectors (`across`,
/// `spans`) round out the list. Case-insensitive whole-word match.
pub const MULTI_SUBSYSTEM_HINTS: &[&str] = &[
    "across",
    "spans",
    "engine",
    "cli",
    "protocol",
    "app-macos",
    "cube",
    "bossctl",
];

/// §Q4 rule 5 marker list — "title matches a mechanical-edit
/// marker → `trivial`." Case-insensitive whole-word match against
/// the title (the design specifies title-only; we widen to title +
/// description for the audit so the denominator counts the same
/// way the report's match-counter does — see Q4 follow-up §"What
/// this is NOT" / the report-shape example which lists `cursor`
/// matches by total appearance, not by title-only.).
pub const MECHANICAL_EDIT_MARKERS: &[&str] = &[
    "rename",
    "apply",
    "revert",
    "bump",
    "move",
    "delete",
    "remove",
    "hide",
    "show",
    "pad",
    "align",
    "re-export",
    "gap",
    "cursor",
    "badge",
    "tooltip",
];

/// Above this fraction of `escalations / matches`, the audit report
/// annotates the marker with "consider promoting" — i.e. the marker
/// is firing for rows workers commonly judge bigger than the level
/// it picks. 0.30 = 30%; tune here (one constants module per the
/// design's open question 2) when retuning the marker lists.
pub const UNDER_CLASS_PROMOTE_THRESHOLD: f64 = 0.30;

/// Below this fraction AND above [`WELL_CLASSIFIED_VOLUME_FLOOR`]
/// matches, the audit report annotates the marker as well-classified
/// ("marker holds; level correct"). 0.05 = 5%.
pub const WELL_CLASSIFIED_RATE_CEILING: f64 = 0.05;

/// Minimum match volume for the "marker holds" callout. Below this
/// floor the rate is too noisy to call. Five matches is a single
/// sprint's worth of mono chores per the design's Appendix A
/// frequency notes.
pub const WELL_CLASSIFIED_VOLUME_FLOOR: u32 = 5;

/// The original-level a marker maps to per §Q4. Used by the audit
/// report to label each row with the level the heuristic *would*
/// have picked when the marker fired in isolation. Returns `None`
/// for unknown markers (e.g. a stale entry on a recorded event whose
/// marker has since been removed from the design).
pub fn original_level_for_marker(marker: &str) -> Option<EffortLevel> {
    let m = marker.to_ascii_lowercase();
    if INVESTIGATE_MARKERS.iter().any(|x| *x == m) {
        Some(EffortLevel::Large)
    } else if MULTI_SUBSYSTEM_HINTS.iter().any(|x| *x == m) {
        Some(EffortLevel::Medium)
    } else if MECHANICAL_EDIT_MARKERS.iter().any(|x| *x == m) {
        Some(EffortLevel::Trivial)
    } else {
        None
    }
}

/// Lowercase iterator over every marker in the §Q4 corpus, in
/// rule-2 → rule-4 → rule-5 order. The audit report uses this to
/// scan a chore's title + description and count which markers
/// matched it.
pub fn all_markers() -> impl Iterator<Item = &'static str> {
    INVESTIGATE_MARKERS
        .iter()
        .chain(MULTI_SUBSYSTEM_HINTS.iter())
        .chain(MECHANICAL_EDIT_MARKERS.iter())
        .copied()
}

/// True iff `text` contains `marker` as a whole-word match,
/// case-insensitive. "Whole word" follows the design's
/// `\b<marker>\b` framing: marker characters bordered on each side
/// by either start/end of string OR a non-alphanumeric, non-`-`
/// character. The dash is preserved because §Q4's `end-to-end`,
/// `re-export`, etc. would otherwise break on the internal hyphen.
pub fn marker_matches_text(marker: &str, text: &str) -> bool {
    if marker.is_empty() || text.len() < marker.len() {
        return false;
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_marker = marker.to_ascii_lowercase();
    let bytes = lower_text.as_bytes();
    let needle = lower_marker.as_bytes();
    let mut start = 0;
    while let Some(pos) = lower_text[start..].find(&lower_marker) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_marker_word_char(bytes[abs - 1]);
        let after_idx = abs + needle.len();
        let after_ok = after_idx >= bytes.len() || !is_marker_word_char(bytes[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_marker_word_char(b: u8) -> bool {
    // The markers themselves contain ASCII alphanumerics, dashes
    // (`end-to-end`, `re-export`), and spaces (`root cause`). For
    // the boundary test we treat alphanumerics and `-` as "inside a
    // word"; space and punctuation count as boundaries. This keeps
    // `rename` from matching `prerender` and `cursor` from matching
    // `precursor`, but lets `cursor.` and `cursor,` match.
    b.is_ascii_alphanumeric() || b == b'_'
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
        assert_eq!(trivial.model, "haiku");
        assert_eq!(trivial.claude_effort, Some("low"));
        assert_eq!(trivial.prompt_addendum, None);

        let small = resolve_spawn_config(Some(EffortLevel::Small), None, None);
        assert_eq!(small.model, "sonnet");
        assert_eq!(small.claude_effort, Some("medium"));
        assert_eq!(small.prompt_addendum, None);

        let medium = resolve_spawn_config(Some(EffortLevel::Medium), None, None);
        assert_eq!(medium.model, "sonnet");
        assert_eq!(medium.claude_effort, Some("high"));
        assert!(
            medium.prompt_addendum.unwrap().starts_with("Sketch"),
            "medium addendum should be the 'sketch a plan' nudge",
        );

        let large = resolve_spawn_config(Some(EffortLevel::Large), None, None);
        assert_eq!(large.model, "opus");
        assert_eq!(large.claude_effort, Some("xhigh"));
        assert!(large.prompt_addendum.unwrap().starts_with("Begin with"));

        let max = resolve_spawn_config(Some(EffortLevel::Max), None, None);
        assert_eq!(max.model, "opus");
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
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn null_row_invocation_matches_today_plus_explicit_model() {
        // Untagged rows fall through to ENGINE_DEFAULT_MODEL (Opus). Must
        // carry --permission-mode auto (Opus) and no --effort.
        let cfg = resolve_spawn_config(None, None, None);
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model opus --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn settings_path_is_threaded_as_settings_flag_before_prompt() {
        // When a worker settings path is supplied it must appear as
        // `--settings '<path>'`, positioned before the trailing prompt
        // arg so claude parses it as a flag, and single-quoted so a
        // path with spaces survives the pane shell.
        let cfg = resolve_spawn_config(None, None, None);
        let path = Path::new("/var/folders/ab/Tmp Dir/boss-worker-settings/mono-agent-003.json");
        let inv = cfg.claude_invocation(false, Some(path));
        assert!(
            inv.contains(
                "--settings '/var/folders/ab/Tmp Dir/boss-worker-settings/mono-agent-003.json'"
            ),
            "expected single-quoted --settings flag, got: {inv:?}",
        );
        let settings_at = inv.find("--settings").expect("--settings present");
        let prompt_at = inv.find("\"$(cat").expect("prompt arg present");
        assert!(
            settings_at < prompt_at,
            "--settings must come before the positional prompt arg: {inv:?}",
        );
    }

    #[test]
    fn trivial_invocation_includes_both_flags() {
        // Sonnet is non-Opus → --dangerously-skip-permissions (default/personal laptop).
        let cfg = resolve_spawn_config(Some(EffortLevel::Trivial), None, None);
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model haiku --effort low --dangerously-skip-permissions \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn medium_with_override_uses_override_model_and_medium_effort() {
        // model_override = "opus" → Opus family → --permission-mode auto.
        let cfg = resolve_spawn_config(Some(EffortLevel::Medium), Some("opus"), None);
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model opus --effort high --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    // --- permission-mode branching ---

    #[test]
    fn opus_model_always_gets_permission_mode_auto() {
        // Opus gets --permission-mode auto regardless of non_opus_auto_mode.
        for model in ["claude-opus-4-7", "claude-opus-4-5", "opus"] {
            for non_opus_auto_mode in [false, true] {
                let cfg = SpawnConfig {
                    effort_level: None,
                    claude_effort: None,
                    model: model.to_owned(),
                    prompt_addendum: None,
                };
                let inv = cfg.claude_invocation(non_opus_auto_mode, None);
                assert!(
                    inv.contains("--permission-mode auto"),
                    "Opus model {model:?} must carry --permission-mode auto, got: {inv:?}",
                );
                assert!(
                    !inv.contains("--dangerously-skip-permissions"),
                    "Opus model {model:?} must NOT carry --dangerously-skip-permissions, got: {inv:?}",
                );
            }
        }
    }

    #[test]
    fn non_opus_model_skip_mode_gets_dangerously_skip_permissions() {
        // non_opus_auto_mode=false (default/personal laptop): --dangerously-skip-permissions.
        for model in [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
        ] {
            let cfg = SpawnConfig {
                effort_level: None,
                claude_effort: None,
                model: model.to_owned(),
                prompt_addendum: None,
            };
            let inv = cfg.claude_invocation(false, None);
            assert!(
                inv.contains("--dangerously-skip-permissions"),
                "Non-Opus model {model:?} with skip mode must carry --dangerously-skip-permissions, got: {inv:?}",
            );
            assert!(
                !inv.contains("--permission-mode"),
                "Non-Opus model {model:?} with skip mode must NOT carry --permission-mode, got: {inv:?}",
            );
        }
    }

    #[test]
    fn non_opus_model_auto_mode_gets_permission_mode_auto() {
        // non_opus_auto_mode=true (corp laptop): --permission-mode auto for Sonnet/Haiku too.
        for model in [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
        ] {
            let cfg = SpawnConfig {
                effort_level: None,
                claude_effort: None,
                model: model.to_owned(),
                prompt_addendum: None,
            };
            let inv = cfg.claude_invocation(true, None);
            assert!(
                inv.contains("--permission-mode auto"),
                "Non-Opus model {model:?} with auto mode must carry --permission-mode auto, got: {inv:?}",
            );
            assert!(
                !inv.contains("--dangerously-skip-permissions"),
                "Non-Opus model {model:?} with auto mode must NOT carry --dangerously-skip-permissions, got: {inv:?}",
            );
        }
    }

    #[test]
    fn model_is_opus_recognises_all_opus_variants() {
        assert!(model_is_opus("claude-opus-4-7"));
        assert!(model_is_opus("claude-opus-4-5"));
        assert!(model_is_opus("opus"));
        assert!(model_is_opus("OPUS"));
        assert!(model_is_opus("Claude-Opus-4-7"));
    }

    #[test]
    fn model_is_opus_rejects_non_opus_models() {
        assert!(!model_is_opus("claude-haiku-4-5-20251001"));
        assert!(!model_is_opus("claude-sonnet-4-6"));
        assert!(!model_is_opus("haiku"));
        assert!(!model_is_opus("sonnet"));
        assert!(!model_is_opus(""));
    }

    #[test]
    fn marker_matches_text_is_case_insensitive_whole_word() {
        assert!(marker_matches_text("rename", "Rename the auth middleware"));
        assert!(marker_matches_text("rename", "RENAME everything"));
        assert!(marker_matches_text("rename", "fix typo: rename, then commit"));
        // Whole-word boundary: 'prerender' does not contain 'rename'.
        assert!(!marker_matches_text("rename", "prerender the static pages"));
        // Hyphenated markers from §Q4 stay intact.
        assert!(marker_matches_text("end-to-end", "Instrument end-to-end traces"));
        assert!(marker_matches_text("re-export", "re-export the public types"));
        // Multi-word markers.
        assert!(marker_matches_text("root cause", "Diagnose the root cause"));
        // Avoid sub-word collisions in the cursor / precursor case.
        assert!(marker_matches_text("cursor", "fix cursor flicker"));
        assert!(!marker_matches_text("cursor", "the precursor design"));
        // Empty haystack / needle.
        assert!(!marker_matches_text("", "anything"));
        assert!(!marker_matches_text("rename", ""));
    }

    #[test]
    fn original_level_for_marker_partitions_q4_corpus() {
        assert_eq!(
            original_level_for_marker("investigate"),
            Some(EffortLevel::Large)
        );
        assert_eq!(
            original_level_for_marker("end-to-end"),
            Some(EffortLevel::Large)
        );
        assert_eq!(
            original_level_for_marker("engine"),
            Some(EffortLevel::Medium)
        );
        assert_eq!(
            original_level_for_marker("RENAME"),
            Some(EffortLevel::Trivial)
        );
        // Stale-marker safety net.
        assert_eq!(original_level_for_marker("not-a-marker"), None);
    }

    #[test]
    fn all_markers_covers_every_q4_rule() {
        let total = INVESTIGATE_MARKERS.len()
            + MULTI_SUBSYSTEM_HINTS.len()
            + MECHANICAL_EDIT_MARKERS.len();
        assert_eq!(all_markers().count(), total);
    }

}
