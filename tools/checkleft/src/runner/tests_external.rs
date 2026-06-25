/// Build a resolved `declarative` external package for routing/policy tests.
/// The mock executor returns canned results, so the invocation body is never
/// actually run.
fn declarative_package(check_id: &str) -> ExternalCheckPackage {
    let manifest = r#"
id = "CHECK_ID"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.md"]

[needs.tool.default]
path = "bazel-bin/checks/domain_typo/domain_typo"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "findings", default = "error" }

[invocations.transform]
kind = "passthrough"
"#
    .replace("CHECK_ID", check_id);
    parse_external_check_package_manifest(&manifest).expect("valid declarative manifest")
}

#[tokio::test]
async fn runner_reports_missing_external_package() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(
        results[0].findings[0]
            .message
            .contains("was not found in configured providers")
    );
}

#[tokio::test]
async fn runner_reports_external_package_id_mismatch() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "different-id".to_owned(),
            runtime: "component-v1".to_owned(),
            api_version: "v1".to_owned(),
            implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                artifact_bytes: None,
                check_name: "different-id".to_owned(),
                limits: None,
                checks: None,
                provenance: None,
            }),
        }),
    };

    let runner = Runner::with_external_package_provider(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("id mismatch"));
}

#[tokio::test]
async fn runner_executes_external_package_via_executor() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "component-v1".to_owned(),
            api_version: "v1".to_owned(),
            implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                artifact_bytes: None,
                check_name: "domain-typo-check".to_owned(),
                limits: None,
                checks: None,
                provenance: None,
            }),
        }),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "external ran".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings.len(), 1);
    // No policy severity specified → default is Error (strict-by-default).
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert_eq!(results[0].findings[0].message, "external ran");

    let seen_packages = seen_packages.lock().expect("lock seen packages").clone();
    assert_eq!(seen_packages, vec!["domain-typo-check".to_owned()]);
}

#[tokio::test]
async fn runner_allows_declarative_runtime_for_local_config() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(declarative_package("domain-typo-check")),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "local declarative ran".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].message, "local declarative ran");
    assert_eq!(
        seen_packages.lock().expect("lock seen packages").as_slice(),
        ["domain-typo-check"]
    );
}

#[tokio::test]
async fn runner_applies_policy_severity_override_to_external_results() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"

[checks.policy]
severity = "error"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "component-v1".to_owned(),
            api_version: "v1".to_owned(),
            implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                artifact_bytes: None,
                check_name: "domain-typo-check".to_owned(),
                limits: None,
                checks: None,
                provenance: None,
            }),
        }),
    };
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "external warning".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::new(Mutex::new(Vec::new())),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
}

#[tokio::test]
async fn runner_applies_bypass_to_external_results() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "component-v1".to_owned(),
            api_version: "v1".to_owned(),
            implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                artifact_bytes: None,
                check_name: "domain-typo-check".to_owned(),
                limits: None,
                checks: None,
                provenance: None,
            }),
        }),
    };
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "external error".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::new(Mutex::new(Vec::new())),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: Path::new("docs/file.md").to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }])
    .with_commit_description(Some("BYPASS_DOMAIN_TYPO=temporary external parity coverage".to_owned()));

    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
    assert!(
        results[0].findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("temporary external parity coverage"))
    );
}

#[tokio::test]
async fn runner_maps_external_executor_failures_to_findings() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "component-v1".to_owned(),
            api_version: "v1".to_owned(),
            implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                artifact_bytes: None,
                check_name: "domain-typo-check".to_owned(),
                limits: None,
                checks: None,
                provenance: None,
            }),
        }),
    };
    let executor = StaticExternalExecutor {
        result: None,
        error_message: Some("sandbox runtime failed".to_owned()),
        seen_packages: Arc::new(Mutex::new(Vec::new())),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("sandbox runtime failed"));
}

#[test]
fn list_configured_checks_reports_external_resolution_errors() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let error = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect_err("must fail");

    assert!(error.to_string().contains("failed to resolve external check packages"));
}

#[tokio::test]
async fn runner_rejects_declarative_runtime_from_external_checks_url() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/shared/CHECKS.yaml"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            r#"
checks:
  - id: domain-typo
    check: domain-typo-check
    implementation: generated:domain-typo-check
"#,
        ))
        .mount(&server)
        .await;

    let provider = StaticExternalProvider {
        package: Some(declarative_package("domain-typo-check")),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: Vec::new(),
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        crate::config::ConfigResolverOptions {
            external_checks_file: None,
            external_checks_url: Some(format!("{}/shared/CHECKS.yaml", server.uri())),
        },
    )
    .await
    .expect("resolver");

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(resolver),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert!(
        results[0].findings[0]
            .message
            .contains("cannot use runtime `declarative-v1`")
    );
    assert!(seen_packages.lock().expect("lock seen packages").is_empty());
}

#[tokio::test]
async fn runner_allows_declarative_runtime_from_external_checks_file() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");

    let external_path = temp.path().join("shared/CHECKS.yaml");
    fs::create_dir_all(external_path.parent().expect("shared dir")).expect("create shared dir");
    fs::write(
        &external_path,
        r#"
checks:
  - id: domain-typo
    check: domain-typo-check
    implementation: generated:domain-typo-check
"#,
    )
    .expect("write external config");

    let provider = StaticExternalProvider {
        package: Some(declarative_package("domain-typo-check")),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "external file declarative ran".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let resolver = ConfigResolver::new_with_options(
        temp.path(),
        crate::config::ConfigResolverOptions {
            external_checks_file: Some(external_path.display().to_string()),
            external_checks_url: None,
        },
    )
    .await
    .expect("resolver");

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(resolver),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].message, "external file declarative ran");
    assert_eq!(
        seen_packages.lock().expect("lock seen packages").as_slice(),
        ["domain-typo-check"]
    );
}

#[test]
fn list_configured_checks_deduplicates() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let checks = runner
        .list_configured_checks(&ChangeSet::new(vec![
            ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: Path::new("backend/src/b.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ]))
        .expect("list checks");

    let check_map: BTreeMap<_, _> = checks
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect();
    assert_eq!(check_map.len(), 2);
    assert!(check_map.contains_key("file-size"));
    assert!(check_map.contains_key("spelling-typos"));
}

/// Build a declarative package with a json transform that uses a dynamic
/// severity template (`{{item.sev}}`). This exercises `preserve_finding_severity`.
fn dynamic_severity_package(check_id: &str) -> ExternalCheckPackage {
    let manifest = r#"
id = "CHECK_ID"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.js"]

[needs.eslint.default]
path = "eslint"

[[invocations]]
id = "run"
run = "eslint"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "ok", "1" = "findings", default = "error" }

[invocations.transform]
kind = "json"
select = ".[]"

[invocations.transform.finding]
path = "{{item.path}}"
message = "{{item.msg}}"
severity = "{{item.sev}}"
"#
    .replace("CHECK_ID", check_id);
    parse_external_check_package_manifest(&manifest).expect("valid dynamic-severity declarative manifest")
}

#[tokio::test]
async fn runner_preserves_dynamic_per_finding_severity_when_no_policy_override() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("create dirs");
    fs::write(temp.path().join("src/app.js"), "console.log('hi');\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "lint/js"
check = "lint/js"
implementation = "generated:lint/js"
config.config_file = "eslint.config.js"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(dynamic_severity_package("lint/js")),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "lint/js".to_owned(),
            findings: vec![
                Finding {
                    severity: Severity::Error,
                    message: "no-unused-vars: 'x' is defined but never used.".to_owned(),
                    location: Some(Location {
                        path: std::path::PathBuf::from("src/app.js"),
                        line: Some(1),
                        column: Some(1),
                    }),
                    remediations: vec![],
                    suggested_fix: None,
                },
                Finding {
                    severity: Severity::Warning,
                    message: "no-console: Unexpected console statement.".to_owned(),
                    location: Some(Location {
                        path: std::path::PathBuf::from("src/app.js"),
                        line: Some(1),
                        column: Some(1),
                    }),
                    remediations: vec![],
                    suggested_fix: None,
                },
            ],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: std::path::Path::new("src/app.js").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    let findings = &results[0].findings;
    assert_eq!(findings.len(), 2, "expected 2 findings; got: {findings:?}");

    let errors: Vec<_> = findings.iter().filter(|f| f.severity == Severity::Error).collect();
    let warnings: Vec<_> = findings.iter().filter(|f| f.severity == Severity::Warning).collect();
    assert_eq!(
        errors.len(),
        1,
        "expected 1 error; severities: {:?}",
        findings.iter().map(|f| f.severity).collect::<Vec<_>>()
    );
    assert_eq!(
        warnings.len(),
        1,
        "expected 1 warning; severities: {:?}",
        findings.iter().map(|f| f.severity).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn runner_policy_severity_override_flattens_dynamic_severity_findings() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("create dirs");
    fs::write(temp.path().join("src/app.js"), "console.log('hi');\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "lint/js"
check = "lint/js"
implementation = "generated:lint/js"
config.config_file = "eslint.config.js"

[checks.policy]
severity = "warning"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(dynamic_severity_package("lint/js")),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "lint/js".to_owned(),
            findings: vec![
                Finding {
                    severity: Severity::Error,
                    message: "no-unused-vars: 'x' is defined but never used.".to_owned(),
                    location: Some(Location {
                        path: std::path::PathBuf::from("src/app.js"),
                        line: Some(1),
                        column: Some(1),
                    }),
                    remediations: vec![],
                    suggested_fix: None,
                },
                Finding {
                    severity: Severity::Error,
                    message: "no-console: Unexpected console statement.".to_owned(),
                    location: Some(Location {
                        path: std::path::PathBuf::from("src/app.js"),
                        line: Some(1),
                        column: Some(1),
                    }),
                    remediations: vec![],
                    suggested_fix: None,
                },
            ],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: std::path::Path::new("src/app.js").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    let findings = &results[0].findings;
    assert_eq!(findings.len(), 2, "expected 2 findings; got: {findings:?}");

    for finding in findings {
        assert_eq!(
            finding.severity,
            Severity::Warning,
            "explicit policy severity=warning must flatten all findings; got: {:?}",
            finding.severity
        );
    }
}

// ── eligible file count progress reporter tests ─────────────────────────────

/// Records `(check_id, registered_total)` for every `register()` call.
#[derive(Default)]
struct CapturingProgressReporter {
    registered: Arc<Mutex<Vec<(String, usize)>>>,
}

impl crate::progress::ProgressReporter for CapturingProgressReporter {
    fn register(&self, check_id: &str, total_files: usize) {
        self.registered
            .lock()
            .expect("lock registered")
            .push((check_id.to_owned(), total_files));
    }
    fn start(&self, _check_id: &str) {}
    fn start_fix(&self, _check_id: &str, _pass: u32) {}
    fn record_progress(&self, _check_id: &str, _processed: usize) {}
    fn finish(&self, _check_id: &str, _files_failed: usize, _elapsed: std::time::Duration) {}
    fn stream_findings(&self, _result: &crate::output::CheckResult) {}
}

/// A test executor that implements `eligible_file_count` properly for declarative
/// packages (using the actual glob filter) and returns empty results for `execute`.
struct DeclarativeEligibleCountExecutor {
    root: std::path::PathBuf,
}

impl ExternalCheckExecutor for DeclarativeEligibleCountExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        _changeset: &ChangeSet,
        _source_tree: &dyn crate::input::SourceTree,
        _config: &toml::Value,
        _config_dir: &std::path::Path,
        _effective_severity: Option<crate::output::Severity>,
        _exclusion: &crate::exclusion_matcher::ExclusionMatcher,
    ) -> anyhow::Result<crate::output::CheckResult> {
        Ok(crate::output::CheckResult {
            check_id: package.id.clone(),
            findings: vec![],
        })
    }

    fn eligible_file_count(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> usize {
        match &package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => {
                crate::external::declarative::eligible_file_count(&self.root, d, changeset, config)
            }
            _ => changeset.changed_files.len(),
        }
    }
}

fn rs_only_declarative_package(check_id: &str) -> ExternalCheckPackage {
    let manifest = format!(
        r#"
id = "{check_id}"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.rs"]

[needs.tool.default]
path = "rustfmt"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#
    );
    parse_external_check_package_manifest(&manifest).expect("valid declarative manifest")
}

fn bazel_only_declarative_package(check_id: &str) -> ExternalCheckPackage {
    let manifest = format!(
        r#"
id = "{check_id}"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/BUILD", "**/*.bzl", "**/BUILD.bazel"]

[needs.tool.default]
path = "buildifier"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#
    );
    parse_external_check_package_manifest(&manifest).expect("valid declarative manifest")
}

fn js_only_declarative_package(check_id: &str) -> ExternalCheckPackage {
    let manifest = format!(
        r#"
id = "{check_id}"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.js", "**/*.ts"]

[needs.eslint.default]
path = "eslint"

[[invocations]]
id = "run"
run = "eslint"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#
    );
    parse_external_check_package_manifest(&manifest).expect("valid declarative manifest")
}

/// Run `run_changeset_with_progress` with three declarative checks (rs-only,
/// bazel-only, js-only) against a mixed-file changeset. Verifies that the
/// progress reporter `register()` is called with the per-check eligible count
/// (post-applies_to filtering), not the global file count.
#[tokio::test]
async fn runner_registers_eligible_file_count_per_declarative_check() {
    let temp = tempdir().expect("create temp dir");

    // Write a CHECKS config that wires all three declarative checks.
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/rust"
check = "format/rust"
implementation = "generated:format/rust"

[[checks]]
id = "format/bazel"
check = "format/bazel"
implementation = "generated:format/bazel"

[[checks]]
id = "lint/js"
check = "lint/js"
implementation = "generated:lint/js"
"#,
    )
    .expect("write config");

    // Mixed changeset: 2 .rs, 2 bazel, 1 .ts, 3 other files → 8 total.
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::Path::new("src/main.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("src/lib.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("BUILD").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("tools/defs.bzl").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("src/app.ts").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("README.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("config.json").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::Path::new("Makefile").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    // Provider returns each package by check id.
    let rust_pkg = rs_only_declarative_package("format/rust");
    let bazel_pkg = bazel_only_declarative_package("format/bazel");
    let js_pkg = js_only_declarative_package("lint/js");

    // Provider resolves by check id (not implementation ref). The CHECKS.toml has
    // `check = "format/rust"` / `implementation = "generated:format/rust"`, so the
    // provider is asked for the generated ref — match by the id suffix.
    struct ByCheckIdProvider {
        rust: ExternalCheckPackage,
        bazel: ExternalCheckPackage,
        js: ExternalCheckPackage,
    }
    impl ExternalCheckPackageProvider for ByCheckIdProvider {
        fn resolve(
            &self,
            impl_ref: &crate::external::ExternalCheckImplementationRef,
        ) -> anyhow::Result<Option<ExternalCheckPackage>> {
            let key = impl_ref.to_string();
            if key.contains("format/rust") {
                Ok(Some(self.rust.clone()))
            } else if key.contains("format/bazel") {
                Ok(Some(self.bazel.clone()))
            } else if key.contains("lint/js") {
                Ok(Some(self.js.clone()))
            } else {
                Ok(None)
            }
        }
    }

    let provider = Arc::new(ByCheckIdProvider {
        rust: rust_pkg,
        bazel: bazel_pkg,
        js: js_pkg,
    });
    let executor = Arc::new(DeclarativeEligibleCountExecutor {
        root: temp.path().to_path_buf(),
    });

    let runner = Runner::with_external(
        Arc::new(crate::check::CheckRegistry::new()),
        Arc::new(crate::config::ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(crate::source_tree::LocalSourceTree::new(temp.path()).expect("tree")),
        provider,
        executor,
    );

    let registered = Arc::new(Mutex::new(Vec::new()));
    let reporter = Arc::new(CapturingProgressReporter {
        registered: Arc::clone(&registered),
    });

    runner
        .run_changeset_with_progress(&changeset, reporter)
        .await
        .expect("run checks");

    let registered = registered.lock().expect("lock registered").clone();
    let reg_map: std::collections::HashMap<String, usize> = registered.into_iter().collect();

    assert_eq!(
        reg_map.get("format/rust").copied(),
        Some(2),
        "format/rust must register 2 (only .rs files), got: {reg_map:?}"
    );
    assert_eq!(
        reg_map.get("format/bazel").copied(),
        Some(2),
        "format/bazel must register 2 (BUILD + .bzl), got: {reg_map:?}"
    );
    assert_eq!(
        reg_map.get("lint/js").copied(),
        Some(1),
        "lint/js must register 1 (.ts file), got: {reg_map:?}"
    );
}

/// `repo-visibility` is a built-in check that skips non-BUILD files.
/// Its `applicable_file_count()` must report only BUILD files, not the global
/// changeset size, so the progress UI shows the accurate eligible count.
#[tokio::test]
async fn runner_registers_eligible_file_count_for_builtin_filtering_check() {
    let temp = tempdir().expect("create temp dir");

    // Wire the built-in repo-visibility check; no `implementation` field needed.
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "repo-visibility"
"#,
    )
    .expect("write config");

    // Mixed changeset: 2 BUILD files + 3 non-BUILD files → 5 total, 2 applicable.
    fs::create_dir_all(temp.path().join("src")).expect("create src dir");
    fs::create_dir_all(temp.path().join("tools")).expect("create tools dir");
    fs::write(temp.path().join("src/BUILD"), "").expect("write BUILD");
    fs::write(temp.path().join("tools/BUILD.bazel"), "").expect("write BUILD.bazel");
    fs::write(temp.path().join("src/main.rs"), "fn main() {}").expect("write rs");
    fs::write(temp.path().join("README.md"), "# readme").expect("write md");
    fs::write(temp.path().join("Makefile"), "all:").expect("write makefile");

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: Path::new("src/BUILD").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("tools/BUILD.bazel").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("src/main.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("README.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("Makefile").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register checks");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let registered = Arc::new(Mutex::new(Vec::new()));
    let reporter = Arc::new(CapturingProgressReporter {
        registered: Arc::clone(&registered),
    });

    runner
        .run_changeset_with_progress(&changeset, reporter)
        .await
        .expect("run checks");

    let registered = registered.lock().expect("lock registered").clone();
    let reg_map: std::collections::HashMap<String, usize> = registered.into_iter().collect();

    assert_eq!(
        reg_map.get("repo-visibility").copied(),
        Some(2),
        "repo-visibility must register 2 (only BUILD files in changeset), got: {reg_map:?}"
    );
}
