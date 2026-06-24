// Regression tests: checkleft fix --all must converge in a single invocation
// when a formatter requires multiple passes. The root cause was that
// dispatch_fix called run_declarative_fixes with max_passes.unwrap_or(1), so
// a 2-pass formatter would leave files in an intermediate state (the "still
// failing" bug). The fix changed the default to unwrap_or(DEFAULT_FIX_PASSES).

/// Guard that DEFAULT_FIX_PASSES is large enough to converge a 2-pass
/// formatter. Fails at compile time if the constant is reverted to 1.
const _: () = {
    use super::DEFAULT_FIX_PASSES;
    assert!(
        DEFAULT_FIX_PASSES >= 2,
        "DEFAULT_FIX_PASSES must be >= 2 so that a 2-pass formatter \
         converges when --max-passes is omitted from dispatch_fix"
    );
};

#[cfg(unix)]
#[test]
fn run_declarative_fixes_converges_in_multiple_passes() {
    use std::os::unix::fs::PermissionsExt;

    use crate::external::parse_declarative_check_manifest;
    use crate::progress::NoopProgressReporter;

    let temp = tempdir().expect("temp dir");

    // Write a file whose content transitions: "original" -> "intermediate" ->
    // "fixed". Two fixer passes are required to reach the stable final state.
    fs::write(temp.path().join("test.md"), b"original").expect("write test file");

    // Fixer script: one content-state transition per invocation.
    let fixer_path = {
        let path = temp.path().join("fixer.sh");
        fs::write(
            &path,
            b"#!/bin/sh\n\
for f in \"$@\"; do\n\
  content=$(cat \"$f\")\n\
  case \"$content\" in\n\
    original) printf 'intermediate' > \"$f\" ;;\n\
    intermediate) printf 'fixed' > \"$f\" ;;\n\
  esac\n\
done\n\
exit 0\n",
        )
        .expect("write fixer script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
        path
    };

    // Build the declarative package manifest with the absolute fixer path.
    // Use concat! to embed the {{files}} template literally without extra escapes.
    let manifest = format!(
        concat!(
            "id: test/multipass\n",
            "mode: declarative\n",
            "runtime: declarative-v1\n",
            "api_version: v1\n",
            "applies_to: [\"**/*.md\"]\n",
            "needs:\n",
            "  fixer:\n",
            "    default:\n",
            "      path: \"{fixer}\"\n",
            "invocations:\n",
            "  - id: format\n",
            "    run: fixer\n",
            "    mode: batch\n",
            "    args: [\"{{{{files}}}}\"]\n",
            "    exit:\n",
            "      \"0\": ok\n",
            "      \"1\": findings\n",
            "      default: error\n",
            "    transform:\n",
            "      kind: linelist\n",
            "      message: \"needs formatting\"\n",
            "    fix:\n",
            "      args: [\"{{{{files}}}}\"]\n",
            "      exit:\n",
            "        \"0\": ok\n",
            "        default: error\n",
        ),
        fixer = fixer_path.display()
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");

    // CHECKS.toml that registers the check for all .md files.
    fs::write(
        temp.path().join("CHECKS.toml"),
        b"[[checks]]\nid = \"test/multipass\"\ncheck = \"test/multipass\"\nimplementation = \"generated:test/multipass\"\n",
    )
    .expect("write CHECKS.toml");

    let check_id = "test/multipass";
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("test.md"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let fix_plan: BTreeMap<String, Vec<PathBuf>> = [(check_id.to_owned(), vec![PathBuf::from("test.md")])]
        .into_iter()
        .collect();

    let build_runner = || {
        Runner::with_external_package_provider(
            Arc::new(CheckRegistry::new()),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
            Arc::new(StaticExternalProvider {
                package: Some(package.clone()),
            }),
        )
    };

    // With max_passes=1 the fixer only runs once, leaving the file in the
    // intermediate state (same behaviour as dispatch_fix had before the fix,
    // when it called run_declarative_fixes with max_passes.unwrap_or(1)).
    build_runner()
        .run_declarative_fixes(&changeset, &fix_plan, temp.path(), 1, Arc::new(NoopProgressReporter))
        .expect("run fixes");

    assert_eq!(
        fs::read(temp.path().join("test.md")).expect("read file"),
        b"intermediate",
        "max_passes=1: file is only partially fixed (one pass applied)"
    );

    // Reset to the original state.
    fs::write(temp.path().join("test.md"), b"original").expect("reset file");

    // With max_passes=10 the loop iterates until no more changes occur,
    // so the file reaches the fully converged final state.
    build_runner()
        .run_declarative_fixes(&changeset, &fix_plan, temp.path(), 10, Arc::new(NoopProgressReporter))
        .expect("run fixes");

    assert_eq!(
        fs::read(temp.path().join("test.md")).expect("read file"),
        b"fixed",
        "max_passes=10: file converges to final state across multiple passes"
    );
}
