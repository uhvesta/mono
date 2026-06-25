use std::path::Path;

use checkleft::starlark::testing::{StarlarkTestOptions, run_package_tests};
use runfiles::Runfiles;

#[test]
fn runs_checked_in_text_package_fixtures() {
    let runfiles = Runfiles::create().expect("create runfiles");
    let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".to_owned());
    let fixture_root = runfiles
        .rlocation(format!(
            "{workspace}/tools/checkleft/tests/fixtures/starlark_text_package"
        ))
        .expect("resolve fixture root");
    let result = run_package_tests(&fixture_root, Path::new("checkleft"), &StarlarkTestOptions::default())
        .expect("run Starlark package tests");

    assert_eq!(result.cases.len(), 2);
    for case in result.cases {
        assert!(
            case.passed,
            "{} / {}: {:?}",
            case.check_id, case.case_name, case.message
        );
    }
}
