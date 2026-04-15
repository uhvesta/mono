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
    assert!(
        diagnostics[0]
            .message
            .contains("failed to parse checks config")
    );
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
        checks
            .get("external-only")
            .expect("external-only present")
            .origin,
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
            .contains("external checks files may only use `generated:` implementations")
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

    assert!(
        error
            .to_string()
            .contains("returned 404 Not Found after 5 attempts")
    );
}
