//! Missing-file-tolerant line/grep readers over rotated logs.
//!
//! These are the primitives `bossctl logs tail` / `follow` go through. They
//! are deliberately file-scan-only and never touch the engine RPC, so they
//! work even when the engine is wedged.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;

use crate::segments::segments_with_live;

/// Read every line of `path` matching the optional `grep` substring filter.
/// A missing file is treated as empty (not an error), since a log may not
/// exist yet on a freshly installed engine.
pub fn read_file_lines(path: &Path, grep: Option<&str>) -> Result<Vec<String>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let lines = std::io::BufReader::new(file)
        .lines()
        .map_while(std::io::Result::ok)
        .filter(|line| grep.is_none_or(|g| line.contains(g)))
        .collect();
    Ok(lines)
}

/// Collect the last `tail_n` lines across the live file and all rotated
/// segments of `base_path`. Segments are read oldest-first (see
/// [`segments_with_live`]) so the returned slice is in chronological order.
pub fn collect_tail_lines(
    base_path: &Path,
    tail_n: usize,
    grep: Option<&str>,
) -> Result<Vec<String>> {
    let mut all_lines: Vec<String> = Vec::new();
    for seg in segments_with_live(base_path) {
        all_lines.extend(read_file_lines(&seg, grep)?);
    }
    let start = all_lines.len().saturating_sub(tail_n);
    Ok(all_lines[start..].to_vec())
}

/// Read bytes appended to `path` since `from_pos`. Returns the new (filtered)
/// lines and the byte offset of the end of the last complete line consumed.
/// A partial trailing line (no newline yet) is left for the next poll so a
/// half-written JSON record is never emitted.
pub fn read_new_content(
    path: &Path,
    from_pos: u64,
    grep: Option<&str>,
) -> Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(from_pos))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if buf.is_empty() {
        return Ok((Vec::new(), from_pos));
    }
    let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
        Some(pos) => pos,
        None => return Ok((Vec::new(), from_pos)),
    };
    let new_pos = from_pos + last_nl as u64 + 1;
    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    let lines: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| grep.is_none_or(|g| l.contains(g)))
        .map(|s| s.to_owned())
        .collect();
    Ok((lines, new_pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segments::rotated_segment_path;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn read_file_lines_returns_all_lines_without_filter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello world").unwrap();
        writeln!(f, "foo bar").unwrap();
        let lines = read_file_lines(&path, None).unwrap();
        assert_eq!(lines, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn read_file_lines_filters_by_grep() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello world").unwrap();
        writeln!(f, "foo bar").unwrap();
        writeln!(f, "hello again").unwrap();
        let lines = read_file_lines(&path, Some("hello")).unwrap();
        assert_eq!(lines, vec!["hello world", "hello again"]);
    }

    #[test]
    fn read_file_lines_returns_empty_for_missing_file() {
        let path = Path::new("/nonexistent/surely/does/not/exist/test.log");
        let lines = read_file_lines(path, None).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn collect_tail_lines_returns_last_n() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..10u32 {
            writeln!(f, "line {i}").unwrap();
        }
        let lines = collect_tail_lines(&path, 3, None).unwrap();
        assert_eq!(lines, vec!["line 7", "line 8", "line 9"]);
    }

    #[test]
    fn collect_tail_lines_returns_all_when_fewer_than_n() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "only line").unwrap();
        let lines = collect_tail_lines(&path, 50, None).unwrap();
        assert_eq!(lines, vec!["only line"]);
    }

    #[test]
    fn collect_tail_lines_spans_rotated_segments() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // Older rotated segment (lower timestamp).
        let mut old = std::fs::File::create(rotated_segment_path(&base, 1000)).unwrap();
        writeln!(old, "old-line-1").unwrap();
        writeln!(old, "old-line-2").unwrap();
        // Newer rotated segment (higher timestamp).
        let mut newer = std::fs::File::create(rotated_segment_path(&base, 2000)).unwrap();
        writeln!(newer, "newer-line-1").unwrap();
        // Live file.
        let mut live = std::fs::File::create(&base).unwrap();
        writeln!(live, "live-line-1").unwrap();
        writeln!(live, "live-line-2").unwrap();

        // Tail 3 lines — should come from newer + live segments, in order.
        let lines = collect_tail_lines(&base, 3, None).unwrap();
        assert_eq!(lines, vec!["newer-line-1", "live-line-1", "live-line-2"]);
    }

    #[test]
    fn read_new_content_reads_complete_lines_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.log");
        std::fs::write(&path, b"line1\nline2\npartial").unwrap();
        let (lines, pos) = read_new_content(&path, 0, None).unwrap();
        assert_eq!(lines, vec!["line1", "line2"]);
        assert_eq!(pos, b"line1\nline2\n".len() as u64);
    }

    #[test]
    fn read_new_content_returns_nothing_when_no_complete_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.log");
        std::fs::write(&path, b"no newline yet").unwrap();
        let (lines, pos) = read_new_content(&path, 0, None).unwrap();
        assert!(lines.is_empty());
        assert_eq!(pos, 0);
    }

    #[test]
    fn read_new_content_applies_grep_filter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("grep.log");
        std::fs::write(&path, b"match me\nskip this\nmatch too\n").unwrap();
        let (lines, _) = read_new_content(&path, 0, Some("match")).unwrap();
        assert_eq!(lines, vec!["match me", "match too"]);
    }
}
