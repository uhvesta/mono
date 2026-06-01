use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{LogSource, resolve_state_root};

/// Resolve the on-disk path for a log source, using the state root as the
/// base directory. For the audit log the engine's own env-var override
/// (`BOSS_ENGINE_AUDIT_PATH`) is honoured so the CLI and engine always agree
/// on which file they are talking about.
pub(crate) fn resolve_log_source_path(source: &LogSource, state_root: &Path) -> PathBuf {
    match source {
        LogSource::Engine => state_root.join("engine-trace.jsonl"),
        LogSource::Audit => {
            if let Ok(p) = std::env::var(boss_engine::audit::AUDIT_PATH_ENV) {
                let trimmed = p.trim().to_owned();
                if !trimmed.is_empty() {
                    return PathBuf::from(trimmed);
                }
            }
            state_root.join("engine-audit.log")
        }
    }
}

/// Build an oldest-to-newest list of log segments for `base_path`.
///
/// Rotated files use the timestamped format produced by PR #1081:
/// `<base_path>.<unix_seconds>` (e.g. `engine-trace.jsonl.1748694000`).
/// Files are sorted by the numeric timestamp suffix so the result is in
/// chronological order (lowest timestamp = oldest). Any file in the same
/// directory whose name does not match `<base_filename>.<all-digits>` is
/// ignored. The live file (`base_path` itself) is always appended last even
/// if absent — callers handle missing files gracefully via [`read_file_lines`].
pub(crate) fn rotated_segments(base_path: &Path) -> Vec<PathBuf> {
    let mut segments = list_rotated_log_files(base_path);
    // Sort ascending by numeric timestamp so oldest segment comes first.
    segments.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit('.').next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    });
    segments.push(base_path.to_path_buf());
    segments
}

/// Enumerate rotated log files alongside `active_path`.
/// A file qualifies iff its name is `<active_filename>.<all-digits>`.
fn list_rotated_log_files(active_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = active_path.parent() else {
        return vec![];
    };
    let Some(stem) = active_path.file_name().and_then(|n| n.to_str()) else {
        return vec![];
    };
    let prefix = format!("{stem}.");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.filter_map(|e| e.ok())
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

/// Read all lines from `path` that match the optional `grep` filter.
/// A missing file is treated as empty (not an error), since the log may not
/// exist yet on a freshly installed engine.
pub(crate) fn read_file_lines(path: &Path, grep: Option<&str>) -> Result<Vec<String>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let lines = std::io::BufReader::new(file)
        .lines()
        .filter_map(|r| r.ok())
        .filter(|line| grep.is_none_or(|g| line.contains(g)))
        .collect();
    Ok(lines)
}

/// Collect the last `tail_n` lines across the current file and any rotated
/// segments. Segments are read oldest-first so the returned slice is in
/// chronological order.
pub(crate) fn collect_tail_lines(
    base_path: &Path,
    tail_n: usize,
    grep: Option<&str>,
) -> Result<Vec<String>> {
    let mut all_lines: Vec<String> = Vec::new();
    for seg in rotated_segments(base_path) {
        all_lines.extend(read_file_lines(&seg, grep)?);
    }
    let start = all_lines.len().saturating_sub(tail_n);
    Ok(all_lines[start..].to_vec())
}

pub(crate) fn logs_tail(
    json: bool,
    source: LogSource,
    state_root: Option<PathBuf>,
    tail_n: usize,
    grep: Option<&str>,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let base_path = resolve_log_source_path(&source, &root);
    let tail_lines = collect_tail_lines(&base_path, tail_n, grep)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "source": source.to_string(),
                "path": base_path.display().to_string(),
                "lines": tail_lines,
                "count": tail_lines.len(),
            })
        );
    } else if tail_lines.is_empty() {
        eprintln!("==> {} <== (no lines)", base_path.display());
    } else {
        eprintln!("==> {} <==", base_path.display());
        for line in &tail_lines {
            println!("{line}");
        }
    }
    Ok(())
}

/// Read bytes appended to `path` since `from_pos`. Returns the new lines and
/// the byte offset of the end of the last complete line consumed. Partial
/// trailing lines (no newline yet) are left for the next poll so we never
/// emit a half-written JSON record.
pub(crate) fn read_new_content(
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

pub(crate) async fn logs_follow(
    source: LogSource,
    state_root: Option<PathBuf>,
    tail_n: usize,
    grep: Option<String>,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let base_path = resolve_log_source_path(&source, &root);

    let tail_lines = collect_tail_lines(&base_path, tail_n, grep.as_deref())?;
    if !tail_lines.is_empty() {
        eprintln!("==> {} <==", base_path.display());
        for line in &tail_lines {
            println!("{line}");
        }
    }

    let mut pos: u64 = std::fs::metadata(&base_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("==> (following — Ctrl-C to stop) <==");

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        match std::fs::metadata(&base_path) {
            Ok(m) => {
                let new_len = m.len();
                if new_len < pos {
                    // File was rotated or truncated; reset so we catch the new content.
                    pos = 0;
                }
                if new_len > pos {
                    match read_new_content(&base_path, pos, grep.as_deref()) {
                        Ok((lines, new_pos)) => {
                            for line in lines {
                                println!("{line}");
                            }
                            pos = new_pos;
                        }
                        Err(err) => {
                            eprintln!(
                                "bossctl: error reading {}: {err}",
                                base_path.display()
                            );
                        }
                    }
                }
            }
            Err(_) => {
                // File disappeared (e.g. mid-rotation); reset so we read from start when it reappears.
                pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_lines_returns_all_lines_without_filter() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello world").unwrap();
        writeln!(f, "foo bar").unwrap();
        let lines = read_file_lines(&path, None).unwrap();
        assert_eq!(lines, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn read_file_lines_filters_by_grep() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
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
        let path = std::path::Path::new("/nonexistent/surely/does/not/exist/test.log");
        let lines = read_file_lines(path, None).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn collect_tail_lines_returns_last_n() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
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
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "only line").unwrap();
        let lines = collect_tail_lines(&path, 50, None).unwrap();
        assert_eq!(lines, vec!["only line"]);
    }

    #[test]
    fn read_new_content_reads_complete_lines_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("partial.log");
        // Two complete lines followed by a partial (no trailing newline).
        std::fs::write(&path, b"line1\nline2\npartial").unwrap();
        let (lines, pos) = read_new_content(&path, 0, None).unwrap();
        assert_eq!(lines, vec!["line1", "line2"]);
        // pos should point past the second newline, not into the partial line.
        assert_eq!(pos, b"line1\nline2\n".len() as u64);
    }

    #[test]
    fn read_new_content_returns_nothing_when_no_complete_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("partial.log");
        std::fs::write(&path, b"no newline yet").unwrap();
        let (lines, pos) = read_new_content(&path, 0, None).unwrap();
        assert!(lines.is_empty());
        assert_eq!(pos, 0); // position should not advance
    }

    #[test]
    fn read_new_content_applies_grep_filter() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("grep.log");
        std::fs::write(&path, b"match me\nskip this\nmatch too\n").unwrap();
        let (lines, _) = read_new_content(&path, 0, Some("match")).unwrap();
        assert_eq!(lines, vec!["match me", "match too"]);
    }

    #[test]
    fn rotated_segments_ends_with_base_path() {
        let base = std::path::Path::new("/tmp/fake-engine-trace.jsonl");
        let segs = rotated_segments(base);
        assert_eq!(segs.last().unwrap(), base);
    }

    #[test]
    fn rotated_segments_orders_timestamp_files_oldest_first() {
        use std::io::Write as _;
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // Create three rotated files with ascending timestamps.
        for ts in [1000u64, 3000, 2000] {
            let mut f = std::fs::File::create(dir.path().join(format!("engine-trace.jsonl.{ts}")))
                .unwrap();
            writeln!(f, "ts={ts}").unwrap();
        }
        // Create the live file.
        let mut live = std::fs::File::create(&base).unwrap();
        writeln!(live, "live").unwrap();

        let segs = rotated_segments(&base);
        // 4 segments: 3 rotated + 1 live.
        assert_eq!(segs.len(), 4);
        // First three should be in ascending timestamp order.
        assert!(segs[0].to_string_lossy().ends_with(".1000"));
        assert!(segs[1].to_string_lossy().ends_with(".2000"));
        assert!(segs[2].to_string_lossy().ends_with(".3000"));
        // Live file last.
        assert_eq!(&segs[3], &base);
    }

    #[test]
    fn rotated_segments_ignores_non_timestamp_siblings() {
        use std::io::Write as _;
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // A valid rotated file.
        let mut f =
            std::fs::File::create(dir.path().join("engine-trace.jsonl.1748694000")).unwrap();
        writeln!(f, "old").unwrap();
        // Unrelated files that must NOT be included.
        std::fs::write(dir.path().join("engine-trace.jsonl.bak"), b"noise").unwrap();
        std::fs::write(dir.path().join("engine-trace.jsonl.1.gz"), b"noise").unwrap();
        std::fs::write(dir.path().join("other-file.txt"), b"noise").unwrap();

        let segs = rotated_segments(&base);
        // Only the valid timestamp file + live path.
        assert_eq!(segs.len(), 2);
        assert!(segs[0].to_string_lossy().ends_with(".1748694000"));
        assert_eq!(&segs[1], &base);
    }

    #[test]
    fn collect_tail_lines_spans_rotated_segments() {
        use std::io::Write as _;
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("engine-trace.jsonl");
        // Older rotated segment (lower timestamp).
        let mut old =
            std::fs::File::create(dir.path().join("engine-trace.jsonl.1000")).unwrap();
        writeln!(old, "old-line-1").unwrap();
        writeln!(old, "old-line-2").unwrap();
        // Newer rotated segment (higher timestamp).
        let mut newer =
            std::fs::File::create(dir.path().join("engine-trace.jsonl.2000")).unwrap();
        writeln!(newer, "newer-line-1").unwrap();
        // Live file.
        let mut live = std::fs::File::create(&base).unwrap();
        writeln!(live, "live-line-1").unwrap();
        writeln!(live, "live-line-2").unwrap();

        // Tail 3 lines — should come from newer + live segments, in order.
        let lines = collect_tail_lines(&base, 3, None).unwrap();
        assert_eq!(lines, vec!["newer-line-1", "live-line-1", "live-line-2"]);
    }
}
