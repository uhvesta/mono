//! Rotated-segment naming, enumeration, and chronological ordering.
//!
//! Rotated logs use the timestamped scheme introduced in PR #1081:
//! `<base>.<unix_seconds>` (e.g. `engine-trace.jsonl.1748694000`). This module
//! is the only place that encodes that format — the engine writer/pruner and
//! the `bossctl` reader both build on it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in whole seconds since the Unix epoch (`0` if the
/// clock is somehow before the epoch). Used to stamp new rotated segments.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The rotated-segment path for `base` at `secs`: the suffix `.{secs}` is
/// appended to the *full* live filename, yielding `<base>.<unix_seconds>`
/// (e.g. `engine-trace.jsonl` -> `engine-trace.jsonl.1748694000`).
///
/// This is THE definition of the on-disk rotated-segment filename format.
/// Appending to the whole filename (rather than swapping an extension) keeps
/// the live file's own extension intact and guarantees the result satisfies
/// the `<filename>.<all-digits>` predicate that [`rotated_segments`] uses to
/// find these files again.
pub fn rotated_segment_path(base: &Path, secs: u64) -> PathBuf {
    let mut name = base.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{secs}"));
    match base.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

/// The first non-existing rotated path for `base`, stamped with the current
/// time. Equivalent to [`next_rotated_path_from`] seeded with
/// [`now_unix_secs`].
pub fn next_rotated_path(base: &Path) -> PathBuf {
    next_rotated_path_from(base, now_unix_secs())
}

/// The first non-existing rotated path for `base`, starting at `start_secs`
/// and incrementing the suffix by one second on collision. This handles the
/// rare case of multiple rotations within the same wall-clock second without
/// clobbering an existing segment.
pub fn next_rotated_path_from(base: &Path, start_secs: u64) -> PathBuf {
    let mut secs = start_secs;
    loop {
        let candidate = rotated_segment_path(base, secs);
        if !candidate.exists() {
            return candidate;
        }
        secs += 1;
    }
}

/// Every rotated segment file alongside `base`, oldest-first (ascending by the
/// numeric `<unix_seconds>` suffix). A sibling qualifies iff its name is
/// `<base_filename>.<all-digits>`; anything else (`.bak`, `.1.gz`, unrelated
/// files) is ignored. The live file (`base` itself) is **not** included.
///
/// Returns an empty vec when `base` has no parent/filename or the directory
/// cannot be read — callers treat that as "no rotated history".
pub fn rotated_segments(base: &Path) -> Vec<PathBuf> {
    let mut segments = enumerate_segments(base);
    // Sort ascending by the numeric timestamp suffix so the oldest segment
    // comes first. Parsing the suffix (rather than sorting lexicographically)
    // keeps ordering correct even if the timestamp width ever changes.
    segments.sort_by_key(|p| segment_suffix_secs(p));
    segments
}

/// [`rotated_segments`] followed by the live `base` path. The result is the
/// full on-disk history in chronological order, suitable for a reader that
/// scans oldest-to-newest. `base` is always appended last even if it does not
/// exist yet — readers tolerate missing files.
pub fn segments_with_live(base: &Path) -> Vec<PathBuf> {
    let mut segments = rotated_segments(base);
    segments.push(base.to_path_buf());
    segments
}

/// Parse the trailing `.<digits>` suffix of a rotated segment as seconds.
/// Falls back to `0` for anything unparseable (which never reaches here in
/// practice, since [`enumerate_segments`] already filters to all-digit
/// suffixes).
fn segment_suffix_secs(path: &Path) -> u64 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit('.').next())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Enumerate (unsorted) the `<base_filename>.<all-digits>` siblings of `base`.
fn enumerate_segments(base: &Path) -> Vec<PathBuf> {
    let Some(dir) = base.parent() else {
        return vec![];
    };
    let Some(stem) = base.file_name().and_then(|n| n.to_str()) else {
        return vec![];
    };
    let prefix = format!("{stem}.");
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return vec![];
    };
    read_dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let suffix = n.strip_prefix(prefix.as_str()).unwrap_or("");
                    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
                })
                .unwrap_or(false)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn rotated_segment_path_appends_unix_seconds() {
        let base = Path::new("/var/log/engine-trace.jsonl");
        assert_eq!(
            rotated_segment_path(base, 1_748_694_000),
            PathBuf::from("/var/log/engine-trace.jsonl.1748694000")
        );
    }

    #[test]
    fn rotated_segment_path_preserves_full_filename() {
        // Appending (not extension-swapping) keeps `.log` intact, so the
        // result still matches the enumeration predicate.
        let base = Path::new("/var/log/engine-audit.log");
        assert_eq!(
            rotated_segment_path(base, 42),
            PathBuf::from("/var/log/engine-audit.log.42")
        );
    }

    #[test]
    fn next_rotated_path_skips_existing_seconds() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // Occupy second 1000 so the next path must roll to 1001.
        fs::write(rotated_segment_path(&base, 1000), b"x").unwrap();
        let next = next_rotated_path_from(&base, 1000);
        assert_eq!(next, rotated_segment_path(&base, 1001));
        assert!(!next.exists());
    }

    #[test]
    fn rotated_segments_orders_oldest_first() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // Create rotated files out of order.
        for ts in [1000u64, 3000, 2000] {
            fs::write(rotated_segment_path(&base, ts), b"x").unwrap();
        }
        // The live file should NOT appear in rotated_segments.
        fs::write(&base, b"live").unwrap();

        let segs = rotated_segments(&base);
        assert_eq!(segs.len(), 3);
        assert!(segs[0].to_string_lossy().ends_with(".1000"));
        assert!(segs[1].to_string_lossy().ends_with(".2000"));
        assert!(segs[2].to_string_lossy().ends_with(".3000"));
    }

    #[test]
    fn rotated_segments_orders_numerically_not_lexically() {
        // Mixed-width suffixes: lexicographic order would put "1000000000"
        // before "999999999"; numeric order must not.
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        fs::write(rotated_segment_path(&base, 1_000_000_000), b"x").unwrap();
        fs::write(rotated_segment_path(&base, 999_999_999), b"x").unwrap();

        let segs = rotated_segments(&base);
        assert_eq!(segs.len(), 2);
        assert!(segs[0].to_string_lossy().ends_with(".999999999"));
        assert!(segs[1].to_string_lossy().ends_with(".1000000000"));
    }

    #[test]
    fn rotated_segments_ignores_non_timestamp_siblings() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        fs::write(rotated_segment_path(&base, 1_748_694_000), b"old").unwrap();
        // Siblings that must NOT be treated as rotated segments.
        fs::write(dir.path().join("engine-trace.jsonl.bak"), b"noise").unwrap();
        fs::write(dir.path().join("engine-trace.jsonl.1.gz"), b"noise").unwrap();
        fs::write(dir.path().join("engine-trace.jsonl.old"), b"noise").unwrap();
        fs::write(dir.path().join("other-file.txt"), b"noise").unwrap();

        let segs = rotated_segments(&base);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].to_string_lossy().ends_with(".1748694000"));
    }

    #[test]
    fn segments_with_live_appends_base_last() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        fs::write(rotated_segment_path(&base, 2000), b"x").unwrap();
        fs::write(rotated_segment_path(&base, 1000), b"x").unwrap();

        let segs = segments_with_live(&base);
        assert_eq!(segs.len(), 3);
        assert!(segs[0].to_string_lossy().ends_with(".1000"));
        assert!(segs[1].to_string_lossy().ends_with(".2000"));
        assert_eq!(&segs[2], &base);
    }

    #[test]
    fn segments_with_live_includes_base_even_when_no_rotations() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        let segs = segments_with_live(&base);
        assert_eq!(segs, vec![base]);
    }
}
