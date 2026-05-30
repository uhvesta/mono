use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::tempdir;

use crate::external::ExternalCheckImplementationRef;
use crate::output::Severity;

use super::{ConfigResolver, StaleExclusionMode};

mod yaml;

#[test]
fn stale_exclusion_severity_defaults_to_warn_and_inherits() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
stale_exclusion_severity = "error"

[[checks]]
id = "rust-giant-structs-use-builder"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    // The root setting is inherited by descendant directories.
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");
    assert_eq!(checks.stale_exclusion_mode(), StaleExclusionMode::Error);

    // A repo with no setting defaults to Warn.
    let bare = tempdir().expect("create temp dir");
    fs::write(
        bare.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust-giant-structs-use-builder\"\n",
    )
    .expect("write config");
    let bare_resolver = ConfigResolver::new(bare.path()).expect("create resolver");
    assert_eq!(
        bare_resolver
            .resolve_for_file(Path::new("a.rs"))
            .expect("resolve")
            .stale_exclusion_mode(),
        StaleExclusionMode::Warn
    );
}

#[test]
fn per_check_stale_exclusion_severity_override_is_parsed() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "rust-giant-structs-use-builder"

[checks.policy]
stale_exclusion_severity = "off"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("a.rs"))
        .expect("resolve checks");
    let check = checks
        .get("rust-giant-structs-use-builder")
        .expect("check present");
    assert_eq!(
        check.policy.stale_exclusion_mode,
        Some(StaleExclusionMode::Off)
    );
}

#[test]
fn invalid_stale_exclusion_severity_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
stale_exclusion_severity = "loud"

[[checks]]
id = "rust-giant-structs-use-builder"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("a.rs"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("stale_exclusion_severity")),
        "expected a diagnostic about the invalid severity, got {diagnostics:?}"
    );
}

#[test]
fn resolves_single_config_file() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 500

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "spelling-typos"]);
    assert_eq!(
        checks.get("file-size").expect("file-size present").check,
        "file-size"
    );
    assert_eq!(
        checks
            .get("file-size")
            .expect("file-size present")
            .config
            .as_table()
            .expect("file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(500)
    );
}

#[test]
fn merges_hierarchy_and_child_overrides_parent() {
    let temp = tempdir().expect("create temp dir");

    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 500

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 200

[[checks]]
id = "rust-naming"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "rust-naming", "spelling-typos"]);
    assert_eq!(
        checks
            .get("file-size")
            .expect("file-size present")
            .config
            .as_table()
            .expect("file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(200)
    );
}

#[test]
fn caches_ancestor_config_resolution_across_sibling_directories() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create src dir");
    fs::create_dir_all(temp.path().join("backend/tests")).expect("create tests dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let initial = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve backend/src checks");
    let initial_enabled: Vec<_> = initial.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(initial_enabled, vec!["file-size", "spelling-typos"]);

    fs::remove_file(temp.path().join("CHECKS.toml")).expect("remove root config");
    fs::remove_file(temp.path().join("backend/CHECKS.toml")).expect("remove backend config");

    let checks = resolver
        .resolve_for_file(Path::new("backend/tests/lib.rs"))
        .expect("resolve backend/tests checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "spelling-typos"]);
}

#[test]
fn child_can_disable_inherited_check() {
    let temp = tempdir().expect("create temp dir");

    fs::create_dir_all(temp.path().join("backend/generated")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/generated/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
enabled = false
"#,
    )
    .expect("write generated config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/generated/output.rs"))
        .expect("resolve checks");

    let enabled_map: BTreeMap<_, _> = checks
        .iter()
        .map(|check| (check.id.as_str(), check.enabled))
        .collect();
    assert_eq!(enabled_map.get("file-size"), Some(&false));
    assert_eq!(checks.enabled().count(), 0);
}

#[test]
fn supports_instance_id_with_check_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typos"
check = "typo"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");

    let check = checks.get("domain-typos").expect("check exists");
    assert_eq!(check.id, "domain-typos");
    assert_eq!(check.check, "typo");
    assert_eq!(check.implementation, None);
}

#[test]
fn parses_external_check_implementation_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");

    let check = checks.get("domain-typo").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Generated(
            "domain-typo-check".to_owned()
        ))
    );
    assert_eq!(check.policy.severity, None);
    assert_eq!(check.policy.allow_bypass, None);
    assert_eq!(check.policy.bypass_name, None);
}

#[test]
fn rejects_invalid_external_check_implementation_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "../escape/check.toml"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert!(checks.get("domain-typo").is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "domain-typo");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(diagnostics[0].message.contains("invalid `implementation`"));
}

#[test]
fn ignores_invalid_external_check_implementation_for_disabled_checks() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
enabled = false
implementation = "../escape/check.toml"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("domain-typo").expect("check exists");

    assert!(!check.enabled);
    assert_eq!(check.implementation, None);
}

#[test]
fn parses_policy_config_for_enabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_FILE_SIZE_LIMIT"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");

    assert_eq!(check.policy.severity, Some(Severity::Error));
    assert_eq!(check.policy.allow_bypass, Some(true));
    assert_eq!(
        check.policy.bypass_name.as_deref(),
        Some("BYPASS_FILE_SIZE_LIMIT")
    );
}

#[test]
fn normalizes_policy_bypass_name_from_non_prefixed_value() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"

[checks.policy]
bypass_name = "domain-typo"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("domain-typo").expect("check exists");

    assert_eq!(
        check.policy.bypass_name.as_deref(),
        Some("BYPASS_DOMAIN_TYPO")
    );
}

#[test]
fn child_config_overrides_policy_values() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "warning"
allow_bypass = false
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_CUSTOM_CHILD"
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");

    assert_eq!(check.policy.severity, Some(Severity::Error));
    assert_eq!(check.policy.allow_bypass, Some(true));
    assert_eq!(
        check.policy.bypass_name.as_deref(),
        Some("BYPASS_CUSTOM_CHILD")
    );
}

#[test]
fn rejects_invalid_policy_severity_for_enabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "fatal"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert!(checks.get("file-size").is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "file-size");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(diagnostics[0].message.contains("invalid `policy.severity`"));
}

#[test]
fn ignores_invalid_policy_severity_for_disabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
enabled = false

[checks.policy]
severity = "fatal"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");
    assert!(!check.enabled);
    assert_eq!(check.policy.severity, None);
}

#[test]
fn excludes_config_files_by_default() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("CHECKS.toml"))
        .expect("resolve checks");

    assert!(!checks.include_config_files());
}

#[test]
fn allows_opt_in_to_include_config_files() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
include_config_files = true

[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("CHECKS.toml"))
        .expect("resolve checks");

    assert!(checks.include_config_files());
}

#[test]
fn child_config_can_override_include_config_files_setting() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
include_config_files = true
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[settings]
include_config_files = false
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/CHECKS.toml"))
        .expect("resolve checks");

    assert!(!checks.include_config_files());
}

#[test]
fn malformed_toml_reports_diagnostic_instead_of_failing() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
config = { max_lines = [1, 2 }
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert_eq!(checks.enabled().count(), 0);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "checks-config");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(
        diagnostics[0]
            .message
            .contains("failed to parse checks config")
    );
}
