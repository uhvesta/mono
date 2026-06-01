use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,
    #[serde(default)]
    pub remediations: Vec<String>,
    pub suggested_fix: Option<SuggestedFix>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn parse_with_default(raw: Option<&str>, default: Self) -> Self {
        match raw.unwrap_or("").to_ascii_lowercase().as_str() {
            "error" => Self::Error,
            "warning" => Self::Warning,
            "info" => Self::Info,
            _ => default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedFix {
    pub description: String,
    pub edits: Vec<FileEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: PathBuf,
    pub old_text: String,
    pub new_text: String,
}

#[cfg(test)]
mod tests {
    use super::Severity;

    #[test]
    fn parse_with_default_respects_known_values() {
        assert_eq!(
            Severity::parse_with_default(Some("error"), Severity::Warning),
            Severity::Error
        );
        assert_eq!(
            Severity::parse_with_default(Some("warning"), Severity::Error),
            Severity::Warning
        );
        assert_eq!(
            Severity::parse_with_default(Some("info"), Severity::Error),
            Severity::Info
        );
    }

    #[test]
    fn parse_with_default_falls_back_for_unknown_or_missing_values() {
        assert_eq!(
            Severity::parse_with_default(Some("unknown"), Severity::Warning),
            Severity::Warning
        );
        assert_eq!(
            Severity::parse_with_default(None, Severity::Error),
            Severity::Error
        );
    }
}
