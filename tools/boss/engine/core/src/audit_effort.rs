//! Heuristic feedback loop — design §Q4 follow-up (PR #370).
//!
//! Reads the recorded `effort_escalations` events and the chore
//! corpus for one product, and produces a per-marker
//! under-classification report that lets a human retune the §Q4
//! marker lists without a code change.
//!
//! The marker corpus lives in [`crate::effort`] alongside the
//! dispatcher's effort table — same "knobs we expect to tune
//! without a schema change" family per design §Q2 / Q4. The audit
//! is a pure function of (escalation events, chore corpus,
//! thresholds); it does not retune anything itself.

use boss_protocol::{
    EffortAuditMarkerRow, EffortAuditReport, EffortEscalation, EffortLevel,
};

use crate::effort::{
    UNDER_CLASS_PROMOTE_THRESHOLD, WELL_CLASSIFIED_RATE_CEILING, WELL_CLASSIFIED_VOLUME_FLOOR,
    all_markers, marker_matches_text, original_level_for_marker,
};

/// One chore as the audit sees it: just enough to run the marker
/// matcher. The engine layer projects rows out of the `tasks` table
/// into this shape; tests construct it directly.
#[derive(Debug, Clone)]
pub struct ChoreForAudit {
    pub name: String,
    pub description: String,
}

impl ChoreForAudit {
    /// The combined haystack used for marker matching. The design's
    /// §Q4 rule 5 is title-only; rule 2 / rule 4 are title-OR-
    /// description. The audit widens to title + description for
    /// every marker so the denominator is consistent across rules,
    /// and so a description that mentions `cursor` but has it
    /// removed from the title still counts for the audit's "are we
    /// matching this marker often enough to learn from it" check.
    fn haystack(&self) -> String {
        let mut s = String::with_capacity(self.name.len() + self.description.len() + 1);
        s.push_str(&self.name);
        s.push(' ');
        s.push_str(&self.description);
        s
    }
}

/// True iff `new_level` is strictly higher than `original_level` on
/// the `trivial < small < medium < large < max` ordering — the
/// definition of "escalation" used by the report. Equal-level or
/// downgrade events don't count toward under-classification.
fn is_promotion(original: EffortLevel, new: EffortLevel) -> bool {
    level_ordinal(new) > level_ordinal(original)
}

fn level_ordinal(level: EffortLevel) -> u8 {
    match level {
        EffortLevel::Trivial => 0,
        EffortLevel::Small => 1,
        EffortLevel::Medium => 2,
        EffortLevel::Large => 3,
        EffortLevel::Max => 4,
    }
}

/// Build the per-product report.
///
/// `product_id` / `product_slug` are echoed back in the report
/// header. `chores` is the post-filter chore set the audit scans
/// (one entry per non-deleted chore on the product). `events` is
/// the recorded escalation events the report aggregates. The
/// caller is responsible for trimming `events` to the requested
/// window — this function does not look at timestamps.
pub fn build_report(
    product_id: &str,
    product_slug: &str,
    window_days: Option<u32>,
    chores: &[ChoreForAudit],
    events: &[EffortEscalation],
    generated_at: String,
) -> EffortAuditReport {
    let total_chores = chores.len() as u32;

    // For each marker in the §Q4 corpus, count how many chores'
    // haystack matched it. Doing it once per (chore, marker) keeps
    // the matcher's O(text * marker) cost contained to a single
    // pass; the marker corpus is ~34 entries today and the chore
    // count is in the hundreds per product, so this is cheap.
    let mut matches_per_marker: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for chore in chores {
        let haystack = chore.haystack();
        for marker in all_markers() {
            if marker_matches_text(marker, &haystack) {
                *matches_per_marker
                    .entry(marker.to_ascii_lowercase())
                    .or_insert(0) += 1;
            }
        }
    }

    // For each marker the events recorded, count promotions. We do
    // this against the as-recorded marker (lowercased) so a stale
    // marker on an older event still appears in the report — it
    // just won't have a `matches` denominator from the current
    // chore corpus, which the rate calc handles by emitting
    // `under_class_rate: None`.
    let mut escalations_per_marker: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut total_escalations = 0u32;
    for event in events {
        if !is_promotion(event.original_level, event.new_level) {
            // Not an escalation per the report's definition; skip.
            continue;
        }
        total_escalations += 1;
        for marker in &event.markers {
            *escalations_per_marker
                .entry(marker.to_ascii_lowercase())
                .or_insert(0) += 1;
        }
    }

    // Build a row per (marker observed in chores OR in events).
    // Markers with zero matches AND zero escalations are filtered
    // (the report would not learn anything from a row of all
    // zeroes). The §Q4 corpus is the canonical source for
    // `original_level`; markers that exist on an event but have
    // since been removed from the corpus get `Max` as a sentinel
    // ordering value so they sort to the top and the human can
    // notice they're stale. (`Max` is never the heuristic's choice
    // per design §Q1, so the level itself is a tell.)
    let mut seen_markers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    seen_markers.extend(matches_per_marker.keys().cloned());
    seen_markers.extend(escalations_per_marker.keys().cloned());

    let mut rows: Vec<EffortAuditMarkerRow> = seen_markers
        .into_iter()
        .map(|marker| {
            let matches = matches_per_marker.get(&marker).copied().unwrap_or(0);
            let escalations = escalations_per_marker.get(&marker).copied().unwrap_or(0);
            let original_level =
                original_level_for_marker(&marker).unwrap_or(EffortLevel::Max);
            let under_class_rate = if matches == 0 {
                None
            } else {
                Some(escalations as f64 / matches as f64)
            };
            let annotation = annotate(matches, under_class_rate);
            EffortAuditMarkerRow {
                marker,
                original_level,
                matches,
                escalations,
                under_class_rate,
                annotation,
            }
        })
        .filter(|row| row.matches > 0 || row.escalations > 0)
        .collect();

    // Sort by rate descending (the noisy markers first), nulls
    // last, then by escalation count descending for ties / nulls so
    // a marker with 5 escalations but no current matches still sits
    // near the top.
    rows.sort_by(|a, b| {
        let a_rate = a.under_class_rate.unwrap_or(f64::NEG_INFINITY);
        let b_rate = b.under_class_rate.unwrap_or(f64::NEG_INFINITY);
        b_rate
            .partial_cmp(&a_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.escalations.cmp(&a.escalations))
            .then_with(|| a.marker.cmp(&b.marker))
    });

    EffortAuditReport {
        product_id: product_id.to_owned(),
        product_slug: product_slug.to_owned(),
        window_days,
        total_chores,
        total_escalations,
        under_class_threshold: UNDER_CLASS_PROMOTE_THRESHOLD,
        rows,
        generated_at,
    }
}

fn annotate(matches: u32, rate: Option<f64>) -> Option<String> {
    match rate {
        None => None,
        Some(r) if r > UNDER_CLASS_PROMOTE_THRESHOLD => {
            Some("consider promoting".to_owned())
        }
        Some(r)
            if r < WELL_CLASSIFIED_RATE_CEILING
                && matches >= WELL_CLASSIFIED_VOLUME_FLOOR =>
        {
            Some("marker holds; level correct".to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chore(name: &str, description: &str) -> ChoreForAudit {
        ChoreForAudit {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    fn event(
        id: &str,
        original: EffortLevel,
        new: EffortLevel,
        markers: &[&str],
    ) -> EffortEscalation {
        EffortEscalation {
            id: id.to_owned(),
            product_id: "prod_test".to_owned(),
            work_item_id: format!("task_{id}"),
            original_level: original,
            new_level: new,
            markers: markers.iter().map(|s| (*s).to_owned()).collect(),
            rule_id: None,
            created_at: "0".to_owned(),
        }
    }

    #[test]
    fn empty_inputs_produce_a_clean_zero_report() {
        let report = build_report("prod_x", "boss", None, &[], &[], "12345".to_owned());
        assert_eq!(report.total_chores, 0);
        assert_eq!(report.total_escalations, 0);
        assert!(report.rows.is_empty());
    }

    #[test]
    fn rate_is_computed_against_chore_match_count_not_event_count() {
        // Three chores match `cursor`; one of those escalated. The
        // event's markers list also names `cursor`, so the rate is
        // 1/3 = 33.3%, NOT 1/1 (events-only).
        let chores = vec![
            chore("Fix cursor flicker", ""),
            chore("Resize cursor on hover", ""),
            chore("cursor disappears on focus", ""),
        ];
        let events = vec![event("e1", EffortLevel::Trivial, EffortLevel::Small, &["cursor"])];
        let report = build_report("p", "boss", None, &chores, &events, "0".to_owned());
        let row = report
            .rows
            .iter()
            .find(|r| r.marker == "cursor")
            .expect("cursor row present");
        assert_eq!(row.matches, 3);
        assert_eq!(row.escalations, 1);
        let rate = row.under_class_rate.expect("rate present when matches > 0");
        assert!((rate - 1.0 / 3.0).abs() < 1e-9, "rate was {rate}");
    }

    #[test]
    fn promotion_definition_uses_level_ordering() {
        // Trivial → Small counts. Small → Trivial does NOT (a
        // demotion / de-escalation event).
        let chores = vec![chore("Rename helper", "")];
        let events = vec![
            event("e1", EffortLevel::Trivial, EffortLevel::Small, &["rename"]),
            // Same row de-escalating later — must not be counted.
            event("e2", EffortLevel::Small, EffortLevel::Trivial, &["rename"]),
            // Same-level event — also not an escalation.
            event("e3", EffortLevel::Trivial, EffortLevel::Trivial, &["rename"]),
        ];
        let report = build_report("p", "boss", None, &chores, &events, "0".to_owned());
        assert_eq!(report.total_escalations, 1);
        let row = report
            .rows
            .iter()
            .find(|r| r.marker == "rename")
            .expect("rename row present");
        assert_eq!(row.escalations, 1);
    }

    #[test]
    fn rows_sort_by_rate_descending() {
        let chores = vec![
            // `cursor` matches once.
            chore("cursor blink fix", ""),
            // `rename` matches twice.
            chore("rename foo", ""),
            chore("rename bar", ""),
        ];
        // cursor: 1/1 = 100%. rename: 1/2 = 50%.
        let events = vec![
            event("e1", EffortLevel::Trivial, EffortLevel::Medium, &["cursor"]),
            event("e2", EffortLevel::Trivial, EffortLevel::Small, &["rename"]),
        ];
        let report = build_report("p", "boss", None, &chores, &events, "0".to_owned());
        assert_eq!(report.rows[0].marker, "cursor");
        assert_eq!(report.rows[1].marker, "rename");
    }

    #[test]
    fn promote_annotation_fires_above_threshold() {
        // 2/3 = 66.7% > 30% threshold → "consider promoting."
        let chores = vec![
            chore("rename a", ""),
            chore("rename b", ""),
            chore("rename c", ""),
        ];
        let events = vec![
            event("e1", EffortLevel::Trivial, EffortLevel::Medium, &["rename"]),
            event("e2", EffortLevel::Trivial, EffortLevel::Small, &["rename"]),
        ];
        let report = build_report("p", "boss", None, &chores, &events, "0".to_owned());
        let row = report.rows.iter().find(|r| r.marker == "rename").unwrap();
        assert_eq!(row.annotation.as_deref(), Some("consider promoting"));
    }

    #[test]
    fn holds_annotation_fires_when_low_rate_and_high_volume() {
        // 0 escalations against 6 matches; 0% < 5% ceiling and
        // volume (6) >= floor (5) → "marker holds".
        let chores: Vec<_> = (0..6)
            .map(|i| chore(&format!("Investigate path {i}"), ""))
            .collect();
        let report = build_report("p", "boss", None, &chores, &[], "0".to_owned());
        let row = report
            .rows
            .iter()
            .find(|r| r.marker == "investigate")
            .unwrap();
        assert_eq!(
            row.annotation.as_deref(),
            Some("marker holds; level correct"),
        );
    }

    #[test]
    fn low_volume_suppresses_marker_holds_annotation() {
        // 0% rate, but only 3 matches < volume floor (5) → no
        // annotation. Avoids endorsing a marker that simply hasn't
        // appeared often enough to call.
        let chores: Vec<_> = (0..3)
            .map(|i| chore(&format!("Investigate path {i}"), ""))
            .collect();
        let report = build_report("p", "boss", None, &chores, &[], "0".to_owned());
        let row = report
            .rows
            .iter()
            .find(|r| r.marker == "investigate")
            .unwrap();
        assert!(row.annotation.is_none(), "annotation={:?}", row.annotation);
    }

    #[test]
    fn event_with_stale_marker_still_appears_with_max_level() {
        // A marker that was on the §Q4 list at the time of the
        // event but has since been removed. It still appears so
        // the human can see it had escalations, but original_level
        // is `Max` (the sentinel "not in current corpus") and
        // matches is 0.
        let events = vec![event("e1", EffortLevel::Trivial, EffortLevel::Medium, &["legacy-marker"])];
        let report = build_report("p", "boss", None, &[], &events, "0".to_owned());
        let row = report
            .rows
            .iter()
            .find(|r| r.marker == "legacy-marker")
            .unwrap();
        assert_eq!(row.matches, 0);
        assert_eq!(row.escalations, 1);
        assert!(row.under_class_rate.is_none());
        assert_eq!(row.original_level, EffortLevel::Max);
    }
}
