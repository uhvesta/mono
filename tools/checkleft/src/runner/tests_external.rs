/// Build a resolved `declarative` external package (the runtime the former
/// `exec` tier folded into) for routing/policy tests. The mock executor returns
/// canned results, so the invocation body is never actually run.
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
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
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
