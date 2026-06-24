use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;

use crate::input::{DiffHunk, FileDiff, FileLineDelta};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedPatchFileDiff {
    pub file_diff: FileDiff,
    pub line_delta: FileLineDelta,
}

pub(super) fn parse_file_diffs_from_git_patch(patch: &str) -> HashMap<PathBuf, ParsedPatchFileDiff> {
    let mut output = HashMap::new();

    let mut current_old_path: Option<PathBuf> = None;
    let mut current_new_path: Option<PathBuf> = None;
    let mut current_effective_old_path: Option<PathBuf> = None;
    let mut current_effective_new_path: Option<PathBuf> = None;
    let mut current_hunks = Vec::new();
    let mut current_delta = FileLineDelta::default();

    let flush = |old_path: &Option<PathBuf>,
                 new_path: &Option<PathBuf>,
                 hunks: &mut Vec<DiffHunk>,
                 delta: FileLineDelta,
                 output: &mut HashMap<PathBuf, ParsedPatchFileDiff>| {
        let path = new_path.as_ref().or(old_path.as_ref());
        let Some(path) = path else {
            hunks.clear();
            return;
        };

        let file_diff = FileDiff {
            hunks: std::mem::take(hunks),
        };
        output
            .entry(path.clone())
            .and_modify(|existing| {
                existing.line_delta.added_lines = existing.line_delta.added_lines.saturating_add(delta.added_lines);
                existing.line_delta.removed_lines =
                    existing.line_delta.removed_lines.saturating_add(delta.removed_lines);
                existing.file_diff.hunks.extend(file_diff.hunks.clone());
            })
            .or_insert(ParsedPatchFileDiff {
                file_diff,
                line_delta: delta,
            });
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if current_effective_old_path.is_none() {
                current_effective_old_path = current_old_path.clone();
            }
            if current_effective_new_path.is_none() {
                current_effective_new_path = current_new_path.clone();
            }
            flush(
                &current_effective_old_path,
                &current_effective_new_path,
                &mut current_hunks,
                current_delta,
                &mut output,
            );
            current_delta = FileLineDelta::default();
            current_hunks.clear();
            current_effective_old_path = None;
            current_effective_new_path = None;
            (current_old_path, current_new_path) = parse_diff_git_paths(rest);
            continue;
        }

        if let Some(rest) = line.strip_prefix("--- ") {
            current_effective_old_path = parse_patch_path(rest);
            continue;
        }

        if let Some(rest) = line.strip_prefix("+++ ") {
            current_effective_new_path = parse_patch_path(rest);
            continue;
        }

        if line.starts_with("@@") {
            if let Some(hunk) = parse_hunk_header(line) {
                current_hunks.push(hunk);
            }
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            current_delta.added_lines = current_delta.added_lines.saturating_add(1);
            if let Some(hunk) = current_hunks.last_mut() {
                hunk.added_lines = hunk.added_lines.saturating_add(1);
            }
            continue;
        }

        if line.starts_with('-') && !line.starts_with("---") {
            current_delta.removed_lines = current_delta.removed_lines.saturating_add(1);
            if let Some(hunk) = current_hunks.last_mut() {
                hunk.removed_lines = hunk.removed_lines.saturating_add(1);
            }
            continue;
        }
    }

    if current_effective_old_path.is_none() {
        current_effective_old_path = current_old_path;
    }
    if current_effective_new_path.is_none() {
        current_effective_new_path = current_new_path;
    }
    flush(
        &current_effective_old_path,
        &current_effective_new_path,
        &mut current_hunks,
        current_delta,
        &mut output,
    );
    output
}

fn parse_diff_git_paths(rest: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut parts = rest.split_whitespace();
    let old = parts.next().and_then(parse_patch_path);
    let new = parts.next().and_then(parse_patch_path);
    (old, new)
}

fn parse_patch_path(raw: &str) -> Option<PathBuf> {
    if raw == "/dev/null" {
        return None;
    }
    if let Some(stripped) = raw.strip_prefix("a/") {
        return Some(PathBuf::from(stripped));
    }
    if let Some(stripped) = raw.strip_prefix("b/") {
        return Some(PathBuf::from(stripped));
    }
    Some(PathBuf::from(raw))
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let pattern = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid hunk regex");
    let captures = pattern.captures(line)?;

    Some(DiffHunk {
        old_start: captures.get(1)?.as_str().parse().ok()?,
        old_lines: captures
            .get(2)
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(1),
        new_start: captures.get(3)?.as_str().parse().ok()?,
        new_lines: captures
            .get(4)
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(1),
        added_lines: 0,
        removed_lines: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_file_diffs_from_git_patch;

    #[test]
    fn parses_file_diffs_from_git_patch() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
-old
+new
+more
 same
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1 @@
+created
"#,
        );

        let existing = diffs.get(&PathBuf::from("src/lib.rs")).expect("src/lib.rs delta");
        assert_eq!(existing.line_delta.added_lines, 2);
        assert_eq!(existing.line_delta.removed_lines, 1);
        assert_eq!(existing.file_diff.hunks.len(), 1);
        assert_eq!(existing.file_diff.hunks[0].old_start, 1);
        assert_eq!(existing.file_diff.hunks[0].old_lines, 2);
        assert_eq!(existing.file_diff.hunks[0].new_start, 1);
        assert_eq!(existing.file_diff.hunks[0].new_lines, 3);

        let new_file = diffs.get(&PathBuf::from("src/new.rs")).expect("src/new.rs delta");
        assert_eq!(new_file.line_delta.added_lines, 1);
        assert_eq!(new_file.line_delta.removed_lines, 0);
        assert_eq!(new_file.file_diff.hunks[0].old_start, 0);
        assert_eq!(new_file.file_diff.hunks[0].old_lines, 0);
    }

    #[test]
    fn binary_file_hunk_is_skipped_text_hunks_still_parsed() {
        // A patch with a binary file followed by a text file: the binary entry
        // produces no line-delta (no @@ headers), and the text file is still parsed.
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/data.bin b/data.bin
new file mode 100644
index 0000000..1234567
Binary files /dev/null and b/data.bin differ
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1,2 @@
 existing
+new line
"#,
        );

        assert!(
            !diffs.contains_key(&PathBuf::from("data.bin"))
                || diffs
                    .get(&PathBuf::from("data.bin"))
                    .is_some_and(|d| d.line_delta.added_lines == 0 && d.file_diff.hunks.is_empty()),
            "binary file should have no line-delta or no hunks"
        );

        let text_diff = diffs.get(&PathBuf::from("src/lib.rs")).expect("text file diff");
        assert_eq!(text_diff.line_delta.added_lines, 1);
        assert_eq!(text_diff.line_delta.removed_lines, 0);
    }

    #[test]
    fn parses_deleted_file_patch_under_old_path() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
index 1111111..0000000
--- a/src/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-gone
"#,
        );

        let deleted = diffs.get(&PathBuf::from("src/old.rs")).expect("deleted file diff");
        assert_eq!(deleted.line_delta.added_lines, 0);
        assert_eq!(deleted.line_delta.removed_lines, 1);
        assert_eq!(deleted.file_diff.hunks[0].old_start, 1);
        assert_eq!(deleted.file_diff.hunks[0].new_start, 0);
    }
}
