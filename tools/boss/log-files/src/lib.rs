//! Shared log-file path resolution + rotation handling for Boss.
//!
//! This crate is the **single source of truth** for everything both the
//! engine and `bossctl` need to know about Boss's on-disk logs:
//!
//! - **Path resolution** ([`paths`]): which file backs each [`LogSource`]
//!   under a state root, plus the `BOSS_ENGINE_AUDIT_PATH` override and the
//!   default `~/Library/Application Support/Boss` state root.
//! - **Rotated-segment naming + ordering** ([`segments`]): the
//!   `<base>.<unix_seconds>` filename scheme introduced in PR #1081, the
//!   enumeration of those segments alongside a live file, and their
//!   chronological (ascending-timestamp) ordering.
//! - **Line/grep reading** ([`reader`]): missing-file-tolerant readers used
//!   to tail and follow the rotated logs.
//!
//! Before this crate existed, the format lived in two places that had to be
//! kept in lockstep by hand: the engine writer/pruner in
//! `boss-engine`'s `trace_rotation` (which *creates* `<base>.<unix_seconds>`
//! files) and the `bossctl` reader added in PR #1197 (which *consumes* them).
//! They had already drifted on the sort key — the writer sorted segment
//! filenames lexicographically (correct only because the suffix is a
//! fixed-width 10-digit timestamp) while the reader parsed the suffix as a
//! `u64`. Both call sites now route through [`segments`], which sorts
//! numerically, so a single definition governs the format.
//!
//! ## On-disk format is load-bearing
//!
//! Deployed engines have already written rotated files using the
//! `<base>.<unix_seconds>` scheme; existing logs depend on it. The naming in
//! [`segments::rotated_segment_path`] reproduces that exact format. Do not
//! change it without a migration for files already on disk.

mod paths;
mod reader;
mod segments;

pub use paths::{
    AUDIT_PATH_ENV, ENGINE_AUDIT_FILENAME, ENGINE_TRACE_FILENAME, LogSource, audit_path_override,
    default_audit_log_path, default_state_root, resolve_log_source_path,
};
pub use reader::{collect_tail_lines, read_file_lines, read_new_content};
pub use segments::{
    next_rotated_path, next_rotated_path_from, now_unix_secs, rotated_segment_path, rotated_segments,
    segments_with_live,
};
