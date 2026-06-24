//! Conflict-graph scheduling for `checkleft fix`.
//!
//! Builds a conflict graph keyed by fixable-file overlap. Checks with disjoint
//! fixable sets are placed in separate [`FixGroup`]s and can run concurrently;
//! checks sharing any file are placed in the same group and applied serially in
//! category order: **lint before format** (a linter's `--fix` may produce
//! unformatted output, so the formatter must run last to normalise it).

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Category of a check for fix-ordering purposes within a conflict group.
///
/// Determines the relative application order: lint fixes are applied first
/// (they may produce unformatted output), format fixes last (they normalise
/// whatever lint produced). Other checks go in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FixCategory {
    Lint,
    Other,
    Format,
}

impl FixCategory {
    /// Derive the fix category from a check ID by inspecting the leading segment
    /// before the first `/`. For example: `lint/oxc` → `Lint`, `format/rust` →
    /// `Format`, `file/size` → `Other`.
    pub fn from_check_id(check_id: &str) -> Self {
        let prefix = check_id.split('/').next().unwrap_or(check_id);
        match prefix {
            "lint" => Self::Lint,
            "format" => Self::Format,
            _ => Self::Other,
        }
    }
}

/// A group of checks whose fixable-file sets are mutually overlapping (or
/// transitively connected through shared files). Checks in the same group must
/// be applied serially in [`FixGroup::ordered_checks`] order so that each
/// check sandboxes the latest real bytes written by its predecessors.
///
/// Two groups returned by [`build_fix_schedule`] are always **disjoint**: no
/// file appears in both groups. Groups may therefore be applied concurrently.
#[derive(Debug)]
pub struct FixGroup {
    /// Checks to apply, in order: lint first, format last, other checks in
    /// between. Within a category, order is stable by check ID.
    pub ordered_checks: Vec<String>,
}

/// Build the fix schedule from the per-check failing-file map.
///
/// Returns a list of [`FixGroup`]s whose file sets are pairwise disjoint.
/// Each group's checks are sorted by category (lint → other → format) then
/// by check ID within a category, so applying them left-to-right is safe.
///
/// Checks with an empty fixable-file set are still included (they will be
/// no-ops) so that their outcomes are reported to the caller.
pub fn build_fix_schedule(fix_plan: &BTreeMap<String, Vec<PathBuf>>) -> Vec<FixGroup> {
    let check_ids: Vec<String> = fix_plan.keys().cloned().collect();
    let n = check_ids.len();

    if n == 0 {
        return Vec::new();
    }

    // Union-find: each slot is the parent index of a check.
    let mut parent: Vec<usize> = (0..n).collect();

    // For each file, collect which check indices operate on it; union them all
    // into one component so they will be serialised.
    let mut file_to_indices: BTreeMap<&PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, check_id) in check_ids.iter().enumerate() {
        for file in &fix_plan[check_id] {
            file_to_indices.entry(file).or_default().push(i);
        }
    }
    for indices in file_to_indices.values() {
        // Union all checks that share this file into the same component.
        for i in 1..indices.len() {
            union(&mut parent, indices[0], indices[i]);
        }
    }

    // Assign each check to its component root.
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        components.entry(find(&parent, i)).or_default().push(i);
    }

    // Build FixGroups, sorted within each component by (category, check_id).
    let mut groups: Vec<FixGroup> = components
        .into_values()
        .map(|mut indices| {
            indices.sort_by(|&a, &b| {
                let cat_a = FixCategory::from_check_id(&check_ids[a]);
                let cat_b = FixCategory::from_check_id(&check_ids[b]);
                cat_a.cmp(&cat_b).then_with(|| check_ids[a].cmp(&check_ids[b]))
            });
            FixGroup {
                ordered_checks: indices.into_iter().map(|i| check_ids[i].clone()).collect(),
            }
        })
        .collect();

    // Stable-sort groups by their first check's ID for a deterministic schedule.
    groups.sort_by(|a, b| a.ordered_checks[0].cmp(&b.ordered_checks[0]));
    groups
}

fn find(parent: &[usize], mut x: usize) -> usize {
    while parent[x] != x {
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], x: usize, y: usize) {
    let px = find(parent, x);
    let py = find(parent, y);
    if px != py {
        parent[px] = py;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::{FixCategory, build_fix_schedule};

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    fn plan(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<PathBuf>> {
        entries
            .iter()
            .map(|(id, files)| (id.to_string(), paths(files)))
            .collect()
    }

    // ── FixCategory ──────────────────────────────────────────────────────────────

    #[test]
    fn category_lint() {
        assert_eq!(FixCategory::from_check_id("lint/oxc"), FixCategory::Lint);
        assert_eq!(FixCategory::from_check_id("lint/bazel"), FixCategory::Lint);
    }

    #[test]
    fn category_format() {
        assert_eq!(FixCategory::from_check_id("format/rust"), FixCategory::Format);
        assert_eq!(FixCategory::from_check_id("format/prettier"), FixCategory::Format);
    }

    #[test]
    fn category_other() {
        assert_eq!(FixCategory::from_check_id("file/size"), FixCategory::Other);
        assert_eq!(FixCategory::from_check_id("code_patterns"), FixCategory::Other);
    }

    #[test]
    fn category_ordering_is_lint_other_format() {
        assert!(FixCategory::Lint < FixCategory::Other);
        assert!(FixCategory::Other < FixCategory::Format);
        assert!(FixCategory::Lint < FixCategory::Format);
    }

    // ── build_fix_schedule ───────────────────────────────────────────────────────

    #[test]
    fn empty_plan_returns_no_groups() {
        assert!(build_fix_schedule(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn single_check_produces_one_group() {
        let p = plan(&[("lint/oxc", &["a.ts"])]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].ordered_checks, vec!["lint/oxc"]);
    }

    #[test]
    fn disjoint_checks_produce_separate_groups() {
        // lint/oxc operates on .ts files; format/rust on .rs — no overlap.
        let p = plan(&[("format/rust", &["b.rs"]), ("lint/oxc", &["a.ts"])]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 2, "disjoint checks must be in separate groups");
        // Groups are sorted by their first check ID.
        assert_eq!(groups[0].ordered_checks, vec!["format/rust"]);
        assert_eq!(groups[1].ordered_checks, vec!["lint/oxc"]);
    }

    #[test]
    fn overlapping_checks_are_one_group_lint_before_format() {
        let p = plan(&[("format/rust", &["a.rs"]), ("lint/oxc", &["a.rs"])]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 1, "overlapping checks must be serialised into one group");
        assert_eq!(
            groups[0].ordered_checks,
            vec!["lint/oxc", "format/rust"],
            "lint must precede format"
        );
    }

    #[test]
    fn transitive_overlap_unifies_components() {
        // lint/oxc and format/rust share b.rs → same component.
        // lint/bazel has no overlap with either → separate group.
        let p = plan(&[
            ("format/rust", &["b.rs", "c.rs"]),
            ("lint/bazel", &["d.bzl"]),
            ("lint/oxc", &["a.rs", "b.rs"]),
        ]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 2);

        let bazel_group = groups
            .iter()
            .find(|g| g.ordered_checks.contains(&"lint/bazel".to_owned()))
            .expect("lint/bazel must be in a group");
        assert_eq!(bazel_group.ordered_checks, vec!["lint/bazel"]);

        let rs_group = groups
            .iter()
            .find(|g| g.ordered_checks.contains(&"lint/oxc".to_owned()))
            .expect("lint/oxc must be in a group");
        assert_eq!(rs_group.ordered_checks, vec!["lint/oxc", "format/rust"]);
    }

    #[test]
    fn within_category_stable_sort_by_check_id() {
        // Two lint checks on the same file → same group, ordered by ID.
        let p = plan(&[("lint/oxc", &["a.ts"]), ("lint/biome", &["a.ts"])]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 1);
        // "lint/biome" < "lint/oxc" alphabetically → biome first.
        assert_eq!(groups[0].ordered_checks, vec!["lint/biome", "lint/oxc"]);
    }

    #[test]
    fn check_with_empty_fixable_set_is_included() {
        // A check with no failing files cannot share files with anything → own group.
        let p = plan(&[("lint/oxc", &[]), ("format/rust", &["a.rs"])]);
        let groups = build_fix_schedule(&p);
        assert_eq!(groups.len(), 2, "empty-set check must still appear in the schedule");
    }

    #[test]
    fn groups_are_deterministically_ordered() {
        // Running build_fix_schedule twice on the same input must produce identical output.
        let p = plan(&[
            ("format/rust", &["a.rs"]),
            ("lint/oxc", &["b.ts"]),
            ("lint/bazel", &["c.bzl"]),
        ]);
        let g1 = build_fix_schedule(&p);
        let g2 = build_fix_schedule(&p);
        let ids1: Vec<Vec<String>> = g1.iter().map(|g| g.ordered_checks.clone()).collect();
        let ids2: Vec<Vec<String>> = g2.iter().map(|g| g.ordered_checks.clone()).collect();
        assert_eq!(ids1, ids2, "build_fix_schedule must be deterministic");
    }
}
