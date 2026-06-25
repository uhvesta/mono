pub mod sarif;

use std::path::Path;

use tracing::warn;

use crate::output::{Finding, Severity};

/// GitHub's three-level annotation vocabulary, shared across GHA workflow
/// commands, the Check Runs API, and SARIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationLevel {
    Failure,
    Warning,
    Notice,
}

/// A single annotation ready to emit to any GitHub UI surface.
///
/// Produced by `annotation_from_finding`; each backend translates this into its
/// own wire format (GHA `::error::` lines, Check Runs API JSON, SARIF results).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Repo-relative path with forward slashes.
    pub path: String,
    /// 1-based start line. File-level findings (no line) default to 1.
    pub start_line: u32,
    /// Equal to `start_line` today; ranges are deferred to a future task.
    pub end_line: u32,
    /// Only meaningful when `start_line == end_line`.
    pub start_column: Option<u32>,
    pub end_column: Option<u32>,
    pub level: AnnotationLevel,
    /// The check id, e.g. `lint/rust`.
    pub title: String,
    pub message: String,
    /// Same as `title`; the SARIF `ruleId` / deduplication key.
    pub rule_id: String,
}

/// Map a `Finding` to an `Annotation`.
///
/// Returns `None` only when the finding has no location path, which would make
/// a GitHub annotation meaningless (GitHub requires a file path on every
/// annotation).
pub fn annotation_from_finding(check_id: &str, f: &Finding) -> Option<Annotation> {
    let location = f.location.as_ref()?;

    let path = normalize_path(&location.path);

    let (start_line, start_column) = match location.line {
        Some(line) => (line, location.column),
        None => {
            // File-level finding: GitHub requires a line, so default to 1.
            // The annotation lands at the top of the file.
            (1, None)
        }
    };

    let level = severity_to_level(f.severity);

    Some(Annotation {
        path,
        start_line,
        end_line: start_line,
        start_column,
        end_column: None,
        level,
        title: check_id.to_owned(),
        message: f.message.clone(),
        rule_id: check_id.to_owned(),
    })
}

fn severity_to_level(severity: Severity) -> AnnotationLevel {
    match severity {
        Severity::Error => AnnotationLevel::Failure,
        Severity::Warning => AnnotationLevel::Warning,
        Severity::Info => AnnotationLevel::Notice,
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Escape a value for use in the **message** portion of a GHA workflow command.
///
/// Encodes `%` → `%25`, `\r` → `%0D`, `\n` → `%0A`.
pub fn escape_workflow_data(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// Escape a value for use in a **property** of a GHA workflow command
/// (e.g. `file=`, `title=`).
///
/// Encodes the same characters as `escape_workflow_data` plus `:` → `%3A`
/// and `,` → `%2C`.
pub fn escape_workflow_property(s: &str) -> String {
    escape_workflow_data(s).replace(':', "%3A").replace(',', "%2C")
}

/// Truncate `items` to at most `limit` elements, logging a warning when any
/// are dropped.
///
/// `surface` names the backend or context in the log message so callers never
/// silently drop annotations.
pub fn cap_with_log<T>(items: Vec<T>, limit: usize, surface: &str) -> Vec<T> {
    let total = items.len();
    if total <= limit {
        return items;
    }
    let dropped = total - limit;
    warn!(
        "checkleft: {surface} capped at {limit} annotations; \
         {dropped} of {total} findings exceeded the limit and will not appear in the {surface} output"
    );
    items.into_iter().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::output::{Finding, Location, Severity};

    fn make_finding(severity: Severity, message: &str, path: &str, line: Option<u32>, column: Option<u32>) -> Finding {
        Finding {
            severity,
            message: message.to_owned(),
            location: Some(Location {
                path: PathBuf::from(path),
                line,
                column,
            }),
            remediations: vec![],
            suggested_fix: None,
        }
    }

    fn make_finding_no_location(severity: Severity, message: &str) -> Finding {
        Finding {
            severity,
            message: message.to_owned(),
            location: None,
            remediations: vec![],
            suggested_fix: None,
        }
    }

    #[test]
    fn error_maps_to_failure() {
        let f = make_finding(Severity::Error, "msg", "src/main.rs", Some(10), None);
        let a = annotation_from_finding("lint/rust", &f).unwrap();
        assert_eq!(a.level, AnnotationLevel::Failure);
    }

    #[test]
    fn warning_maps_to_warning() {
        let f = make_finding(Severity::Warning, "msg", "a.rs", Some(1), None);
        let a = annotation_from_finding("fmt/rust", &f).unwrap();
        assert_eq!(a.level, AnnotationLevel::Warning);
    }

    #[test]
    fn info_maps_to_notice() {
        let f = make_finding(Severity::Info, "msg", "a.rs", Some(1), None);
        let a = annotation_from_finding("chk", &f).unwrap();
        assert_eq!(a.level, AnnotationLevel::Notice);
    }

    #[test]
    fn finding_with_line_and_column() {
        let f = make_finding(Severity::Error, "bad code", "src/lib.rs", Some(42), Some(7));
        let a = annotation_from_finding("lint/rust", &f).unwrap();
        assert_eq!(a.path, "src/lib.rs");
        assert_eq!(a.start_line, 42);
        assert_eq!(a.end_line, 42);
        assert_eq!(a.start_column, Some(7));
        assert_eq!(a.end_column, None);
        assert_eq!(a.title, "lint/rust");
        assert_eq!(a.rule_id, "lint/rust");
        assert_eq!(a.message, "bad code");
    }

    #[test]
    fn file_level_finding_defaults_to_line_1() {
        let f = make_finding(Severity::Warning, "whole-file issue", "BUILD.bazel", None, None);
        let a = annotation_from_finding("format/bazel", &f).unwrap();
        assert_eq!(a.start_line, 1);
        assert_eq!(a.end_line, 1);
        assert_eq!(a.start_column, None);
    }

    #[test]
    fn finding_without_location_returns_none() {
        let f = make_finding_no_location(Severity::Error, "no location");
        let a = annotation_from_finding("chk", &f);
        assert!(a.is_none());
    }

    #[test]
    fn path_separators_normalized_to_forward_slash() {
        let f = make_finding(Severity::Error, "msg", "tools\\checkleft\\src\\lib.rs", Some(1), None);
        let a = annotation_from_finding("chk", &f).unwrap();
        assert_eq!(a.path, "tools/checkleft/src/lib.rs");
    }

    #[test]
    fn escape_workflow_data_encodes_special_chars() {
        assert_eq!(escape_workflow_data("a%b\rc\nd"), "a%25b%0Dc%0Ad");
    }

    #[test]
    fn escape_workflow_property_also_encodes_colon_and_comma() {
        assert_eq!(escape_workflow_property("a:b,c"), "a%3Ab%2Cc");
    }

    #[test]
    fn escape_workflow_property_encodes_percent_before_colon() {
        // Ensure % is encoded first so the encoded colon/comma are not re-encoded.
        assert_eq!(escape_workflow_property("100%,done:now"), "100%25%2Cdone%3Anow");
    }

    #[test]
    fn cap_with_log_passes_through_when_under_limit() {
        let items: Vec<i32> = (0..5).collect();
        let result = cap_with_log(items.clone(), 10, "test");
        assert_eq!(result, items);
    }

    #[test]
    fn cap_with_log_truncates_and_keeps_first_n() {
        let items: Vec<i32> = (0..20).collect();
        let result = cap_with_log(items, 10, "gha");
        assert_eq!(result.len(), 10);
        assert_eq!(result[0], 0);
        assert_eq!(result[9], 9);
    }

    #[test]
    fn cap_with_log_at_exact_limit_does_not_truncate() {
        let items: Vec<i32> = (0..5).collect();
        let result = cap_with_log(items.clone(), 5, "test");
        assert_eq!(result, items);
    }
}
