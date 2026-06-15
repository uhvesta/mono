#[derive(Clone)]
struct StaticFindingCheck {
    id: String,
    severity: Severity,
    remediation: Option<String>,
}

#[async_trait]
impl Check for StaticFindingCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "emits one static finding"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for StaticFindingCheck {
    async fn run(&self, changeset: &ChangeSet, _tree: &dyn SourceTree) -> Result<CheckResult> {
        let path = changeset
            .changed_files
            .first()
            .map(|changed| changed.path.clone())
            .unwrap_or_else(|| Path::new("unknown").to_path_buf());

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: vec![Finding {
                severity: self.severity,
                message: "synthetic policy finding".to_owned(),
                location: Some(Location {
                    path,
                    line: Some(1),
                    column: Some(1),
                }),
                remediations: self.remediation.iter().cloned().collect(),
                suggested_fix: None,
            }],
        })
    }
}

#[tokio::test]
async fn runner_defaults_to_error_severity_when_no_policy_specified() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Warning,
            remediation: None,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
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
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
}

#[tokio::test]
async fn runner_applies_policy_severity_override() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
severity = "warning"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: None,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
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
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
}

#[tokio::test]
async fn runner_applies_policy_bypass_when_directive_exists() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some("BYPASS_POLICY_CHECK=Legitimate exception.".to_owned())),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
    assert!(results[0].findings[0].message.contains("BYPASS_POLICY_CHECK"));
}

#[tokio::test]
async fn runner_appends_bypass_guidance_when_enabled_and_missing() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
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
    assert_eq!(results[0].findings.len(), 1);
    assert!(
        results[0].findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("never use bypasses for convenience"))
    );
}

#[tokio::test]
async fn runner_ignores_legacy_config_policy_fields() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.config]
severity = "info"
allow_bypass = true
bypass_name = "BYPASS_LEGACY_POLICY_CHECK"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results_without_bypass = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");
    assert_eq!(results_without_bypass[0].findings[0].severity, Severity::Error);
    assert_eq!(
        results_without_bypass[0].findings[0].remediations,
        vec!["fix me".to_owned()]
    );

    let results_with_bypass = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some("BYPASS_LEGACY_POLICY_CHECK=Legacy fallback path.".to_owned())),
        )
        .await
        .expect("run checks");
    assert_eq!(results_with_bypass[0].findings[0].severity, Severity::Error);
    assert_eq!(results_with_bypass[0].findings[0].message, "synthetic policy finding");
}

#[tokio::test]
async fn runner_does_not_apply_bypass_to_runner_generated_errors() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "missing-check"
check = "not-registered"

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
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some(
                "BYPASS_MISSING_CHECK=This should not bypass runner-generated errors.".to_owned(),
            )),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("unknown implementation"));
}
