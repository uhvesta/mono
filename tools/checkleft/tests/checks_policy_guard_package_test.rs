use std::path::Path;

use checkleft::starlark::testing::{StarlarkTestOptions, run_package_tests};
use runfiles::Runfiles;

#[test]
fn checks_policy_guard_fixture_package_passes() {
    let runfiles = Runfiles::create().expect("create runfiles");
    let workspace = std::env::var("TEST_WORKSPACE").expect("TEST_WORKSPACE");
    let fixture_root = runfiles
        .rlocation(format!(
            "{workspace}/tools/checkleft/tests/fixtures/checks_policy_guard"
        ))
        .expect("resolve fixture root");

    let result = run_package_tests(&fixture_root, Path::new("checkleft"), &StarlarkTestOptions::default())
        .expect("run checks policy guard tests");

    assert_eq!(result.cases.len(), 2);
    assert!(
        result.cases.iter().all(|case| case.passed),
        "all cases should pass: {:?}",
        result.cases
    );
}
