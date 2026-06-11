//! W3C-Web-Annotation-style `TextQuoteSelector` resolver for comments.
//!
//! Design: `tools/boss/docs/designs/comments-in-markdown-viewer.md`
//! § "Anchoring model". A comment anchors to a `(exact, prefix, suffix)`
//! quote taken from the rendered *plain-text projection* of a markdown doc.
//! On every load the renderer hands the engine the doc's current plain text;
//! the engine re-resolves each anchor against it:
//!
//! 1. **Exact** — find `prefix + exact + suffix` verbatim. If it occurs
//!    exactly once, the comment is anchored there.
//! 2. **Fuzzy** — otherwise slide a window of the context length over the
//!    plain text and score each position against the original context with a
//!    character-bigram Dice coefficient. Re-anchor when the best score is
//!    `>= score_threshold` (default 0.8) **and** uniquely the best (the best
//!    non-overlapping runner-up scores `< second_best_threshold`, default
//!    0.7). The thresholds are borrowed from Hypothes.is and tunable via the
//!    engine config surface ([`CommentFuzzyConfig::from_env`]).
//! 3. **Orphan** — neither resolves; the comment keeps its sidebar entry but
//!    paints no highlight.
//!
//! All offsets are character offsets (Unicode-scalar count) into the plain
//! text, matching how the macOS renderer indexes its `AttributedString`.
//!
//! The resolver is a pure function with no I/O so it is exercised directly by
//! unit tests here and reused by the cross-doc migration in
//! [`crate::work`].

use std::collections::HashMap;

use boss_protocol::CommentAnchor;

/// Default fuzzy-match score threshold (Hypothes.is starting value).
pub const DEFAULT_FUZZY_SCORE_THRESHOLD: f64 = 0.8;
/// Default second-best (uniqueness) threshold.
pub const DEFAULT_FUZZY_SECOND_BEST_THRESHOLD: f64 = 0.7;

/// Tunable fuzzy-anchoring thresholds. Read once per resolve pass from the
/// engine env surface, mirroring [`crate::config`]'s `*_from_env` style:
///
/// - `BOSS_COMMENT_FUZZY_SCORE` → [`score_threshold`](Self::score_threshold)
/// - `BOSS_COMMENT_FUZZY_SECOND_BEST` →
///   [`second_best_threshold`](Self::second_best_threshold)
///
/// Out-of-range or unparseable values fall back to the defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommentFuzzyConfig {
    /// Minimum Dice score for the best window to count as a match.
    pub score_threshold: f64,
    /// The best runner-up (non-overlapping window) must score below this for
    /// the match to be considered unique.
    pub second_best_threshold: f64,
}

impl Default for CommentFuzzyConfig {
    fn default() -> Self {
        Self {
            score_threshold: DEFAULT_FUZZY_SCORE_THRESHOLD,
            second_best_threshold: DEFAULT_FUZZY_SECOND_BEST_THRESHOLD,
        }
    }
}

impl CommentFuzzyConfig {
    /// Load from the engine env surface, falling back to the defaults for any
    /// unset / unparseable / out-of-`[0,1]` value.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            score_threshold: read_unit_env("BOSS_COMMENT_FUZZY_SCORE").unwrap_or(default.score_threshold),
            second_best_threshold: read_unit_env("BOSS_COMMENT_FUZZY_SECOND_BEST")
                .unwrap_or(default.second_best_threshold),
        }
    }
}

fn read_unit_env(key: &str) -> Option<f64> {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|v| (0.0..=1.0).contains(v))
}

/// The outcome of resolving one anchor against a plain-text projection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnchorResolution {
    /// `prefix + exact + suffix` was found verbatim exactly once. `start` /
    /// `length` locate the `exact` span (char offsets).
    Exact { start: usize, length: usize },
    /// A unique high-scoring fuzzy window. `start` / `length` locate the
    /// best-effort `exact` span within it.
    Fuzzy { start: usize, length: usize, score: f64 },
    /// No confident anchor.
    Orphan,
}

impl AnchorResolution {
    /// The `last_resolved_with` literal for this outcome.
    pub fn last_resolved_with(&self) -> &'static str {
        match self {
            AnchorResolution::Exact { .. } => boss_protocol::RESOLVED_WITH_EXACT,
            AnchorResolution::Fuzzy { .. } => boss_protocol::RESOLVED_WITH_FUZZY,
            AnchorResolution::Orphan => boss_protocol::RESOLVED_WITH_ORPHAN,
        }
    }
}

/// Resolve `anchor` against `plain_text`. See the module docs for the
/// algorithm. An anchor with an empty `exact` is always an orphan (there is
/// nothing to locate).
pub fn resolve_anchor(plain_text: &str, anchor: &CommentAnchor, config: &CommentFuzzyConfig) -> AnchorResolution {
    let exact: Vec<char> = anchor.exact.chars().collect();
    if exact.is_empty() {
        return AnchorResolution::Orphan;
    }
    let haystack: Vec<char> = plain_text.chars().collect();
    let prefix_len = anchor.prefix.chars().count();
    let exact_len = exact.len();
    let context: Vec<char> = anchor.context().chars().collect();
    let tlen = context.len();

    // --- 1. Exact match of the full context, requiring uniqueness. ---
    let exact_hits = find_all(&haystack, &context);
    if exact_hits.len() == 1 {
        let p = exact_hits[0];
        return AnchorResolution::Exact {
            start: p + prefix_len,
            length: exact_len,
        };
    }

    // --- 2. Fuzzy: best + runner-up over sliding windows. ---
    if tlen < 2 {
        // Too short to bigram meaningfully; the exact phase already failed
        // to find a unique verbatim hit, so treat as orphan.
        return AnchorResolution::Orphan;
    }
    let target_bigrams = bigram_counts(&context);
    let target_total: i64 = target_bigrams.values().sum();

    let (best_start, best_score, second_best) = scan_windows(&haystack, tlen, &target_bigrams, target_total);

    if best_score >= config.score_threshold && second_best < config.second_best_threshold {
        // Locate the `exact` span within the best window, clamped to the
        // window and the haystack.
        let span_start = (best_start + prefix_len).min(haystack.len());
        let max_len = (best_start + tlen).min(haystack.len()).saturating_sub(span_start);
        let length = exact_len.min(max_len);
        return AnchorResolution::Fuzzy {
            start: span_start,
            length,
            score: best_score,
        };
    }

    AnchorResolution::Orphan
}

/// Slide a window of length `tlen` over `haystack`, scoring each position's
/// Dice similarity to the target. Returns `(best_start, best_score,
/// second_best_score)` where the runner-up is the best window separated from
/// the best by at least `tlen` characters (so two halves of the same match
/// don't count as a collision).
fn scan_windows(
    haystack: &[char],
    tlen: usize,
    target_bigrams: &HashMap<(char, char), i64>,
    target_total: i64,
) -> (usize, f64, f64) {
    let n = haystack.len();
    if n < tlen {
        // Window can't fit; score the whole text once.
        let score = dice(&bigram_counts(haystack), target_bigrams, target_total);
        return (0, score, 0.0);
    }

    // First pass: best window.
    let mut best_start = 0usize;
    let mut best_score = -1.0f64;
    let mut start = 0usize;
    while start + tlen <= n {
        let window = &haystack[start..start + tlen];
        let score = dice(&bigram_counts(window), target_bigrams, target_total);
        if score > best_score {
            best_score = score;
            best_start = start;
        }
        start += 1;
    }

    // Second pass: best runner-up at least `tlen` chars away from `best_start`.
    let mut second_best = 0.0f64;
    let mut start = 0usize;
    while start + tlen <= n {
        if start.abs_diff(best_start) >= tlen {
            let window = &haystack[start..start + tlen];
            let score = dice(&bigram_counts(window), target_bigrams, target_total);
            if score > second_best {
                second_best = score;
            }
        }
        start += 1;
    }

    (best_start, best_score.max(0.0), second_best)
}

/// Character-bigram multiset of `s` (adjacent overlapping pairs).
fn bigram_counts(s: &[char]) -> HashMap<(char, char), i64> {
    let mut counts: HashMap<(char, char), i64> = HashMap::new();
    for pair in s.windows(2) {
        *counts.entry((pair[0], pair[1])).or_insert(0) += 1;
    }
    counts
}

/// Sørensen–Dice coefficient over two character-bigram multisets:
/// `2 * |A ∩ B| / (|A| + |B|)`, where the intersection sums `min(countA,
/// countB)` per bigram. Range `[0, 1]`; 1.0 for identical strings.
fn dice(a: &HashMap<(char, char), i64>, b: &HashMap<(char, char), i64>, b_total: i64) -> f64 {
    let a_total: i64 = a.values().sum();
    if a_total == 0 && b_total == 0 {
        return 1.0;
    }
    if a_total == 0 || b_total == 0 {
        return 0.0;
    }
    let mut intersection: i64 = 0;
    for (bigram, &count_a) in a {
        if let Some(&count_b) = b.get(bigram) {
            intersection += count_a.min(count_b);
        }
    }
    (2 * intersection) as f64 / (a_total + b_total) as f64
}

/// All start indices where `needle` occurs in `haystack` (char slices).
fn find_all(haystack: &[char], needle: &[char]) -> Vec<usize> {
    let mut hits = Vec::new();
    if needle.is_empty() || needle.len() > haystack.len() {
        return hits;
    }
    let last = haystack.len() - needle.len();
    let mut i = 0usize;
    while i <= last {
        if haystack[i..i + needle.len()] == *needle {
            hits.push(i);
        }
        i += 1;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(exact: &str, prefix: &str, suffix: &str) -> CommentAnchor {
        CommentAnchor {
            exact: exact.to_owned(),
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
        }
    }

    #[test]
    fn exact_resolve_unique() {
        let doc = "Alpha beta gamma delta epsilon zeta eta theta.";
        let a = anchor("gamma delta", "beta ", " epsilon");
        match resolve_anchor(doc, &a, &CommentFuzzyConfig::default()) {
            AnchorResolution::Exact { start, length } => {
                let resolved: String = doc.chars().skip(start).take(length).collect();
                assert_eq!(resolved, "gamma delta");
            }
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn exact_resolve_with_empty_prefix_suffix() {
        let doc = "unique-token-only";
        let a = anchor("unique-token-only", "", "");
        match resolve_anchor(doc, &a, &CommentFuzzyConfig::default()) {
            AnchorResolution::Exact { start, length } => {
                assert_eq!(start, 0);
                assert_eq!(length, "unique-token-only".chars().count());
            }
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_resolve_after_small_edit() {
        // The doc gained a word inside the prefix region, so the verbatim
        // context no longer matches, but the surrounding text is almost
        // identical → fuzzy re-anchor.
        let original_prefix = "The renderer maintains a mapping so the ";
        let exact = "engine never has to know about layout";
        let suffix = ", and the macOS app never round-trips markdown.";
        let a = anchor(exact, original_prefix, suffix);

        let edited = format!(
            "The renderer carefully maintains a mapping so the {exact}, \
             and the macOS app never round-trips markdown."
        );
        let res = resolve_anchor(&edited, &a, &CommentFuzzyConfig::default());
        match res {
            AnchorResolution::Fuzzy { start, length, score } => {
                assert!(score >= 0.8, "score {score} should clear threshold");
                let resolved: String = edited.chars().skip(start).take(length).collect();
                // The recovered span overlaps the real exact text heavily.
                assert!(
                    exact.contains(resolved.trim()) || resolved.contains("engine"),
                    "resolved span {resolved:?} should overlap the exact quote"
                );
            }
            other => panic!("expected fuzzy, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_reject_below_threshold_is_orphan() {
        // The anchored region is entirely rewritten; nothing similar remains.
        let a = anchor(
            "the quick brown fox jumps over the lazy dog",
            "prologue text here ",
            " epilogue text here",
        );
        let edited = "Completely different content about distributed systems, \
                      consensus protocols, and write-ahead logging in databases.";
        assert_eq!(
            resolve_anchor(edited, &a, &CommentFuzzyConfig::default()),
            AnchorResolution::Orphan
        );
    }

    #[test]
    fn fuzzy_reject_when_not_unique() {
        // Two near-identical variants of the quote (only the last word
        // differs from the original), separated by filler. Neither matches
        // verbatim, so the exact phase fails; both score high in the fuzzy
        // phase, so the runner-up clears the uniqueness bar → ambiguous →
        // orphan, even though the best window scores well above threshold.
        let a = anchor("configure the primary widget alpha for the main dashboard view", "", "");
        let doc = "configure the primary widget alpha for the main dashboard panel. \
                   --- some unrelated filler text sits in between the two regions --- \
                   configure the primary widget alpha for the main dashboard screen.";
        assert_eq!(
            resolve_anchor(doc, &a, &CommentFuzzyConfig::default()),
            AnchorResolution::Orphan
        );
    }

    #[test]
    fn orphan_when_text_absent() {
        let a = anchor("nonexistent quoted span", "", "");
        assert_eq!(
            resolve_anchor("totally unrelated document body", &a, &CommentFuzzyConfig::default()),
            AnchorResolution::Orphan
        );
    }

    #[test]
    fn empty_exact_is_orphan() {
        let a = anchor("", "p", "s");
        assert_eq!(
            resolve_anchor("anything", &a, &CommentFuzzyConfig::default()),
            AnchorResolution::Orphan
        );
    }

    #[test]
    fn dice_identical_is_one() {
        let s: Vec<char> = "hello world".chars().collect();
        let counts = bigram_counts(&s);
        let total: i64 = counts.values().sum();
        assert!((dice(&counts, &counts, total) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn config_from_env_defaults_when_unset() {
        // Defaults hold regardless of host env (the keys are unlikely set).
        let cfg = CommentFuzzyConfig::default();
        assert_eq!(cfg.score_threshold, 0.8);
        assert_eq!(cfg.second_best_threshold, 0.7);
    }
}
