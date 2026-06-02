//! `bossctl logs` — read the engine's on-disk logs (trace + audit).
//!
//! All log-path resolution, rotated-segment enumeration/ordering, and the
//! line/grep readers live in `boss-log-files`, the single source of truth
//! shared with the engine. This module is just the CLI orchestration:
//! resolving the state root, printing, and the `--follow` poll loop.
//!
//! Resolving the audit path used to reach into `boss_engine::audit` for the
//! `BOSS_ENGINE_AUDIT_PATH` constant, coupling the CLI to the engine crate.
//! That constant now lives in `boss-log-files`, so the reader no longer
//! depends on the engine to agree on which file it is reading.

use std::path::PathBuf;

use anyhow::Result;

use boss_log_files::{collect_tail_lines, read_new_content, resolve_log_source_path};

use super::{LogSource, resolve_state_root};

/// Map the CLI's [`LogSource`] onto the shared crate's enum.
fn shared_source(source: &LogSource) -> boss_log_files::LogSource {
    match source {
        LogSource::Engine => boss_log_files::LogSource::EngineTrace,
        LogSource::Audit => boss_log_files::LogSource::Audit,
    }
}

pub(crate) fn logs_tail(
    json: bool,
    source: LogSource,
    state_root: Option<PathBuf>,
    tail_n: usize,
    grep: Option<&str>,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let base_path = resolve_log_source_path(shared_source(&source), &root);
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

pub(crate) async fn logs_follow(
    source: LogSource,
    state_root: Option<PathBuf>,
    tail_n: usize,
    grep: Option<String>,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let base_path = resolve_log_source_path(shared_source(&source), &root);

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
                            eprintln!("bossctl: error reading {}: {err}", base_path.display());
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
