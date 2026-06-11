use std::fs;
use std::path::Path;

use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::{CheckConfigOrigin, ConfigResolver, ConfigResolverOptions};

#[test]
fn resolves_yaml_config_file() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: file-size
    config:
      max_lines: 321
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

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
        Some(321)
    );
}

#[test]
fn malformed_yaml_reports_diagnostic_instead_of_failing() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: file-size
    config:
      max_lines: [1, 2
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert_eq!(checks.enabled().count(), 0);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "checks-config");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.yaml"));
    assert!(diagnostics[0].message.contains("failed to parse checks config"));
}

#[tokio::test]
async fn merges_external_yaml_before_local_root_yaml() {
    let temp = tempdir().expect("create temp dir");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared/CHECKS.yaml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
checks:
  - id: shared-file-size
    config:
      max_lines: 999
  - id: external-only
"#,
        ))
        .mount(&server)
        .await;

    fs::write(
        temp.path().join("CHECKS.yaml"),
        format!(
            r#"
settings:
  external_checks_url: "{}/shared/CHECKS.yaml"
checks:
  - id: shared-file-size
    config:
      max_lines: 321
  - id: local-only
"#,
            server.uri()
        ),
    )
    .expect("write config file");

    let resolver = ConfigResolver::new_with_options(temp.path(), ConfigResolverOptions::default())
        .await
        .expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    assert!(checks.get("external-only").is_some());
    assert!(checks.get("local-only").is_some());
    assert_eq!(
        checks
            .get("shared-file-size")
            .expect("shared-file-size present")
            .config
            .as_table()
            .expect("shared-file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(321)
    );
}

#[tokio::test]
async fn supports_cli_external_checks_url_without_root_config() {
    let temp = tempdir().expect("create temp dir");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared/CHECKS.yaml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
checks:
  - id: external-only
"#,
        ))
        .mount(&server)
        .await;

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: None,
            external_checks_url: Some(format!("{}/shared/CHECKS.yaml", server.uri())),
        },
    )
    .await
    .expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    assert!(checks.get("external-only").is_some());
}

#[tokio::test]
async fn merges_external_checks_file_before_local_root_yaml() {
    let temp = tempdir().expect("create temp dir");
    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
checks:
  - id: shared-file-size
    config:
      max_lines: 999
  - id: external-only
"#,
    )
    .expect("write external config");

    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: shared-file-size
    config:
      max_lines: 321
  - id: local-only
"#,
    )
    .expect("write local config");

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    assert!(checks.get("external-only").is_some());
    assert!(checks.get("local-only").is_some());
    assert_eq!(
        checks.get("external-only").expect("external-only present").origin,
        CheckConfigOrigin::ExternalFile
    );
    assert_eq!(
        checks
            .get("shared-file-size")
            .expect("shared-file-size present")
            .config
            .as_table()
            .expect("shared-file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(321)
    );
}

#[tokio::test]
async fn rejects_file_implementation_from_external_checks_file() {
    let temp = tempdir().expect("create temp dir");
    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
checks:
  - id: domain-typo
    check: domain-typo-check
    implementation: checks/domain_typo.check.toml
"#,
    )
    .expect("write external config");

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("create resolver");
    let error = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect_err("resolution must fail");

    assert!(
        error
            .to_string()
            .contains("external checks files may only use `generated:` or `bundled:` implementations")
    );
}

#[tokio::test]
async fn allows_bundled_implementation_from_external_checks_file() {
    // Bundled defs are embedded in the binary, so a remotely-distributed external
    // checks file may reference them (unlike local `File` refs) — this is the
    // zero-install distribution path.
    let temp = tempdir().expect("create temp dir");
    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
checks:
  - id: format/bazel
    implementation: bundled:format/bazel
"#,
    )
    .expect("write external config");

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolution must succeed");

    let check = checks.get("format/bazel").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(crate::external::ExternalCheckImplementationRef::Bundled(
            "format/bazel".to_owned()
        ))
    );
}

#[tokio::test]
async fn bare_id_in_external_config_resolves_to_bundled() {
    // A bare `id: format/bazel` in an external config resolves to the bundled def
    // automatically — no `implementation:` line needed.
    let temp = tempdir().expect("create temp dir");
    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
checks:
  - id: format/bazel
"#,
    )
    .expect("write external config");

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolution must succeed");

    let check = checks.get("format/bazel").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(crate::external::ExternalCheckImplementationRef::Bundled(
            "format/bazel".to_owned()
        ))
    );
}

#[tokio::test]
async fn rejects_exec_paths_in_external_checks_file() {
    // exec_paths would reach into the consuming repo's local filesystem — forbidden
    // in external configs (same trust rule as local File implementation refs).
    let temp = tempdir().expect("create temp dir");
    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
check_definitions:
  exec_paths:
    - tools/checkleft/checks
checks:
  - id: buildifier
"#,
    )
    .expect("write external config");

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("create resolver");
    let error = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect_err("resolution must fail");

    assert!(
        format!("{error:#}").contains("exec_paths` is not allowed in an external checks config"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn fails_when_external_checks_url_returns_404() {
    let temp = tempdir().expect("create temp dir");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing/CHECKS.yaml"))
        .respond_with(ResponseTemplate::new(404))
        .expect(5)
        .mount(&server)
        .await;

    let error = ConfigResolver::new_with_options(
        temp.path(),
        ConfigResolverOptions {
            external_checks_file: None,
            external_checks_url: Some(format!("{}/missing/CHECKS.yaml", server.uri())),
        },
    )
    .await
    .expect_err("resolver must fail");

    assert!(error.to_string().contains("returned 404 Not Found after 5 attempts"));
}

#[test]
fn parses_forbidden_paths_rules_from_yaml() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.yaml"),
        r#"
checks:
  - id: no-generated-artifacts
    check: forbidden-paths
    config:
      rules:
        - remediation: "Remove the artifact."
          when:
            - added
            - modified
          patterns:
            - "**/target/**"
            - "**/node_modules/**"
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("no-generated-artifacts").expect("check present");
    assert_eq!(check.check, "forbidden-paths");
    let rules = check
        .config
        .as_table()
        .expect("config table")
        .get("rules")
        .expect("rules key")
        .as_array()
        .expect("rules array");
    assert_eq!(rules.len(), 1);
    let rule = rules[0].as_table().expect("rule table");
    assert_eq!(
        rule.get("remediation")
            .expect("remediation")
            .as_str()
            .expect("remediation str"),
        "Remove the artifact."
    );
    let when = rule.get("when").expect("when").as_array().expect("when array");
    assert_eq!(when.len(), 2);
    assert_eq!(when[0].as_str(), Some("added"));
    assert_eq!(when[1].as_str(), Some("modified"));
}
