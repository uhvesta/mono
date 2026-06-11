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
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let check = checks.get("rust-giant-structs-use-builder").expect("check present");
    assert_eq!(check.policy.stale_exclusion_mode, Some(StaleExclusionMode::Off));
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
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
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
    assert_eq!(checks.get("file-size").expect("file-size present").check, "file-size");
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

    let enabled_map: BTreeMap<_, _> = checks.iter().map(|check| (check.id.as_str(), check.enabled)).collect();
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
fn parses_explicit_generated_implementation_reference() {
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

// ── bundled resolution (new shape: id/check name only, no implementation: needed) ──

#[test]
fn bare_id_matching_bundled_name_resolves_to_bundled() {
    // The simplest consumer shape: just an id. No implementation:, no check_definitions.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "buildifier"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("buildifier").expect("check exists");
    assert_eq!(check.check, "buildifier");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("buildifier".to_owned()))
    );
}

#[test]
fn custom_id_with_bundled_check_name_resolves_to_bundled() {
    // Custom instance id + check: pointing at a bundled name.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-buildifier"
check = "buildifier"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("my-buildifier").expect("check exists");
    assert_eq!(check.check, "buildifier");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("buildifier".to_owned()))
    );
}

#[test]
fn unknown_name_without_exec_paths_leaves_implementation_none() {
    // A name that is neither bundled nor in exec_paths stays as None (routes to built-in).
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
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("file-size").expect("check exists");
    assert_eq!(check.implementation, None);
}

#[test]
fn exec_paths_resolves_check_from_on_disk_dir() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a fake check def at checks/my-check/check.yaml.
    let defs_dir = temp.path().join("checks/my-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    // The file just needs to exist; content irrelevant for config resolution.
    fs::write(defs_dir.join("check.yaml"), "id: my-check\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "my-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-check/check.yaml").to_path_buf()
        ))
    );
}

#[test]
fn allow_override_bundled_makes_exec_path_win_over_bundled() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a local copy of the bundled buildifier def.
    let defs_dir = temp.path().join("tools/checkleft/checks/buildifier");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("check.yaml"), "id: buildifier\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["tools/checkleft/checks"]
allow_override_bundled = true

[[checks]]
id = "buildifier"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("buildifier").expect("check exists");
    // The exec-path copy wins over the bundled def.
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("tools/checkleft/checks/buildifier/check.yaml").to_path_buf()
        ))
    );
}

#[test]
fn bundled_wins_over_exec_path_by_default() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a local copy of the bundled buildifier def.
    let defs_dir = temp.path().join("checks/buildifier");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("check.yaml"), "id: buildifier\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "buildifier"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("buildifier").expect("check exists");
    // Bundled wins (allow_override_bundled defaults to false).
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("buildifier".to_owned()))
    );
}

#[test]
fn check_definitions_is_inherited_by_child_configs() {
    let temp = tempdir().expect("create temp dir");
    let defs_dir = temp.path().join("checks/my-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("check.yaml"), "id: my-check\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]
"#,
    )
    .expect("write root config");

    fs::create_dir_all(temp.path().join("sub")).expect("create child dir");
    fs::write(
        temp.path().join("sub/CHECKS.toml"),
        r#"
[[checks]]
id = "my-check"
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("sub/file.rs"))
        .expect("resolve checks");

    let check = checks.get("my-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-check/check.yaml").to_path_buf()
        ))
    );
}

#[test]
fn explicit_bundled_ref_still_works() {
    // Explicit `implementation: bundled:<name>` still resolves correctly.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-buildifier"
check = "buildifier"
implementation = "bundled:buildifier"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("my-buildifier").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("buildifier".to_owned()))
    );
}

#[test]
fn explicit_generated_ref_still_works() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-custom"
implementation = "generated:my-custom"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-custom").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Generated("my-custom".to_owned()))
    );
}

#[test]
fn rejects_invalid_exec_path() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["../escape"]
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0]
            .message
            .contains("invalid `check_definitions.exec_paths`")
    );
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
    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_FILE_SIZE_LIMIT"));
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

    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_DOMAIN_TYPO"));
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
    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_CUSTOM_CHILD"));
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
    assert!(diagnostics[0].message.contains("failed to parse checks config"));
}

#[test]
fn coexisting_yaml_and_toml_produces_violation() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("CHECKS.yaml"), "checks:\n  - id: file-size\n").expect("write CHECKS.yaml");
    fs::write(temp.path().join("CHECKS.toml"), "[[checks]]\nid = \"file-size\"\n").expect("write CHECKS.toml");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert_eq!(diagnostics.len(), 1, "expected exactly one coexistence diagnostic");
    assert_eq!(diagnostics[0].check_id, "checks-config");
    assert!(
        diagnostics[0].message.contains("CHECKS.yaml") && diagnostics[0].message.contains("CHECKS.toml"),
        "diagnostic message should name both files: {}",
        diagnostics[0].message
    );
    assert!(
        diagnostics[0].message.contains("keep exactly one"),
        "diagnostic message should instruct the user to keep one: {}",
        diagnostics[0].message
    );
}

#[test]
fn single_config_file_produces_no_coexistence_violation() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("CHECKS.toml"), "[[checks]]\nid = \"file-size\"\n").expect("write CHECKS.toml");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics for a single config file, got: {diagnostics:?}"
    );
}

#[test]
fn exec_paths_resolves_component_check_from_toml_manifest() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a component-mode check.toml (no yaml present for this check).
    let defs_dir = temp.path().join("checks/my-component-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(
        defs_dir.join("check.toml"),
        r#"
id = "my-component-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/my_component_check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write component manifest");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "my-component-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-component-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-component-check/check.toml").to_path_buf()
        ))
    );
}

#[test]
fn exec_paths_yaml_wins_over_toml_when_both_present() {
    // check.yaml takes precedence over check.toml in the same directory.
    let temp = tempdir().expect("create temp dir");
    let defs_dir = temp.path().join("checks/dual-format-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("check.yaml"), "id: dual-format-check\n").expect("write yaml def");
    fs::write(
        defs_dir.join("check.toml"),
        r#"
id = "dual-format-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/dual.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write toml def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "dual-format-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("dual-format-check").expect("check exists");
    // yaml wins (checked first in find_in_exec_paths)
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/dual-format-check/check.yaml").to_path_buf()
        ))
    );
}
