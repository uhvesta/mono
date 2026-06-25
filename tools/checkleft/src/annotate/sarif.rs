use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::annotate::{Annotation, AnnotationLevel, annotation_from_finding, cap_with_log};
use crate::output::CheckResult;

/// GitHub code-scanning SARIF result cap.
///
/// Results beyond this are rejected by GitHub wholesale (the whole upload fails),
/// so the serializer caps here and warns via `cap_with_log`.
pub const SARIF_RESULT_CAP: usize = 5000;

/// Serialize `results` into a SARIF 2.1.0 JSON document.
///
/// Findings without a location are excluded (SARIF results require a
/// `physicalLocation`; GitHub's code scanning rejects location-less results).
/// Applies `cap_with_log` at `SARIF_RESULT_CAP` before building the document.
pub fn to_sarif(results: &[CheckResult]) -> Value {
    let mut annotations: Vec<Annotation> = results
        .iter()
        .flat_map(|r| {
            r.findings
                .iter()
                .filter_map(|f| annotation_from_finding(&r.check_id, f))
        })
        .collect();

    annotations = cap_with_log(annotations, SARIF_RESULT_CAP, "sarif");

    let rule_ids: BTreeSet<&str> = annotations.iter().map(|a| a.rule_id.as_str()).collect();
    let rules: Vec<Value> = rule_ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "shortDescription": { "text": id }
            })
        })
        .collect();

    let sarif_results: Vec<Value> = annotations.iter().map(annotation_to_sarif_result).collect();

    json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "checkleft",
                        "rules": rules
                    }
                },
                "results": sarif_results
            }
        ]
    })
}

/// Serialize a SARIF 2.1.0 document to `path`.
pub fn write_sarif(results: &[CheckResult], path: &Path) -> Result<()> {
    let doc = to_sarif(results);
    let json_str = serde_json::to_string_pretty(&doc).context("failed to serialize SARIF")?;
    std::fs::write(path, json_str).with_context(|| format!("failed to write SARIF to {}", path.display()))?;
    Ok(())
}

fn annotation_to_sarif_result(a: &Annotation) -> Value {
    let level = annotation_level_to_sarif(&a.level);
    let region = build_region(a);

    json!({
        "ruleId": a.rule_id,
        "level": level,
        "message": { "text": a.message },
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": { "uri": a.path },
                    "region": region
                }
            }
        ]
    })
}

fn annotation_level_to_sarif(level: &AnnotationLevel) -> &'static str {
    match level {
        AnnotationLevel::Failure => "error",
        AnnotationLevel::Warning => "warning",
        AnnotationLevel::Notice => "note",
    }
}

fn build_region(a: &Annotation) -> Value {
    let mut region = json!({ "startLine": a.start_line });
    if a.start_line == a.end_line
        && let Some(col) = a.start_column
    {
        region["startColumn"] = json!(col);
    }
    region
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::output::{Finding, Location, Severity};

    fn make_result(check_id: &str, findings: Vec<Finding>) -> CheckResult {
        CheckResult {
            check_id: check_id.to_owned(),
            findings,
        }
    }

    fn make_finding(severity: Severity, msg: &str, path: &str, line: Option<u32>, col: Option<u32>) -> Finding {
        Finding {
            severity,
            message: msg.to_owned(),
            location: Some(Location {
                path: PathBuf::from(path),
                line,
                column: col,
            }),
            remediations: vec![],
            suggested_fix: None,
        }
    }

    fn make_finding_no_location(severity: Severity, msg: &str) -> Finding {
        Finding {
            severity,
            message: msg.to_owned(),
            location: None,
            remediations: vec![],
            suggested_fix: None,
        }
    }

    #[test]
    fn empty_results_produces_valid_sarif() {
        let doc = to_sarif(&[]);
        assert_eq!(doc["version"], "2.1.0");
        assert_eq!(doc["runs"][0]["results"].as_array().unwrap().len(), 0);
        assert_eq!(doc["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn single_error_finding_maps_to_error_level() {
        let results = vec![make_result(
            "lint/rust",
            vec![make_finding(
                Severity::Error,
                "bad code",
                "src/lib.rs",
                Some(10),
                Some(3),
            )],
        )];
        let doc = to_sarif(&results);
        let result = &doc["runs"][0]["results"][0];
        assert_eq!(result["level"], "error");
        assert_eq!(result["ruleId"], "lint/rust");
        assert_eq!(result["message"]["text"], "bad code");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/lib.rs"
        );
        assert_eq!(result["locations"][0]["physicalLocation"]["region"]["startLine"], 10);
        assert_eq!(result["locations"][0]["physicalLocation"]["region"]["startColumn"], 3);
    }

    #[test]
    fn warning_maps_to_warning_level() {
        let results = vec![make_result(
            "fmt/rust",
            vec![make_finding(
                Severity::Warning,
                "would reformat",
                "src/main.rs",
                Some(1),
                None,
            )],
        )];
        let doc = to_sarif(&results);
        assert_eq!(doc["runs"][0]["results"][0]["level"], "warning");
    }

    #[test]
    fn info_maps_to_note_level() {
        let results = vec![make_result(
            "chk",
            vec![make_finding(Severity::Info, "advisory", "a.rs", Some(5), None)],
        )];
        let doc = to_sarif(&results);
        assert_eq!(doc["runs"][0]["results"][0]["level"], "note");
    }

    #[test]
    fn finding_without_location_is_excluded() {
        let results = vec![make_result(
            "chk",
            vec![make_finding_no_location(Severity::Error, "no location")],
        )];
        let doc = to_sarif(&results);
        assert_eq!(doc["runs"][0]["results"].as_array().unwrap().len(), 0);
        assert_eq!(doc["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn rules_catalog_contains_unique_check_ids() {
        let results = vec![
            make_result(
                "lint/rust",
                vec![
                    make_finding(Severity::Error, "e1", "a.rs", Some(1), None),
                    make_finding(Severity::Warning, "w1", "b.rs", Some(2), None),
                ],
            ),
            make_result(
                "lint/rust",
                vec![make_finding(Severity::Error, "e2", "c.rs", Some(3), None)],
            ),
            make_result(
                "fmt/bazel",
                vec![make_finding(Severity::Warning, "w2", "BUILD", Some(1), None)],
            ),
        ];
        let doc = to_sarif(&results);
        let rules = doc["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        let ids: Vec<&str> = rules.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"lint/rust"));
        assert!(ids.contains(&"fmt/bazel"));
    }

    #[test]
    fn results_count_matches_findings_with_location() {
        let results = vec![make_result(
            "chk",
            vec![
                make_finding(Severity::Error, "e1", "a.rs", Some(1), None),
                make_finding_no_location(Severity::Error, "no loc"),
                make_finding(Severity::Warning, "w1", "b.rs", Some(2), None),
            ],
        )];
        let doc = to_sarif(&results);
        // Only 2 of 3 findings have a location.
        assert_eq!(doc["runs"][0]["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn file_level_finding_defaults_to_line_1_no_column() {
        let results = vec![make_result(
            "chk",
            vec![make_finding(Severity::Warning, "whole file", "BUILD.bazel", None, None)],
        )];
        let doc = to_sarif(&results);
        let region = &doc["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["startLine"], 1);
        assert!(region.get("startColumn").is_none() || region["startColumn"].is_null());
    }

    #[test]
    fn column_present_when_line_given() {
        let results = vec![make_result(
            "chk",
            vec![make_finding(Severity::Error, "msg", "src/lib.rs", Some(5), Some(12))],
        )];
        let doc = to_sarif(&results);
        let region = &doc["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["startLine"], 5);
        assert_eq!(region["startColumn"], 12);
    }

    #[test]
    fn cap_truncates_at_sarif_result_cap() {
        let findings: Vec<Finding> = (0..SARIF_RESULT_CAP + 10)
            .map(|i| make_finding(Severity::Error, "msg", "a.rs", Some(i as u32 + 1), None))
            .collect();
        let results = vec![make_result("chk", findings)];
        let doc = to_sarif(&results);
        assert_eq!(doc["runs"][0]["results"].as_array().unwrap().len(), SARIF_RESULT_CAP);
    }

    #[test]
    fn path_separators_normalized_to_forward_slash() {
        let results = vec![make_result(
            "chk",
            vec![make_finding(
                Severity::Error,
                "msg",
                "tools\\checkleft\\src\\lib.rs",
                Some(1),
                None,
            )],
        )];
        let doc = to_sarif(&results);
        let uri = doc["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
            .as_str()
            .unwrap();
        assert_eq!(uri, "tools/checkleft/src/lib.rs");
    }

    #[test]
    fn sarif_schema_and_version_are_correct() {
        let doc = to_sarif(&[]);
        assert_eq!(doc["version"], "2.1.0");
        assert_eq!(doc["$schema"], "https://json.schemastore.org/sarif-2.1.0.json");
        assert_eq!(doc["runs"][0]["tool"]["driver"]["name"], "checkleft");
    }
}
