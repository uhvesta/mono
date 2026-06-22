// Host-side bindings generated from the `checkleft:check@0.1.0` WIT package.
//
// `wasmtime::component::bindgen!` reads `wit/check.wit` at compile time and
// emits Rust types for every WIT record, enum, and variant, plus a trait
// (`CheckPre` / `Check`) that the host uses to call `list-checks` and
// `run-check` on a loaded component.
//
// In production code these bindings are used by the component executor. Their
// presence here also acts as a compile-time proof that the WIT is syntactically
// valid and generates coherent Rust types.
wasmtime::component::bindgen!({
    world: "check",
    path: "wit/check.wit",
});

#[cfg(test)]
mod tests {
    use super::checkleft::check::types;

    // Verify that the generated types are usable: construct a minimal
    // `CheckDescriptor` and round-trip a `CheckError`. This exercises the
    // lifted WIT types without requiring a real wasm component.

    #[test]
    fn check_descriptor_can_be_constructed() {
        let desc = types::CheckDescriptor {
            name: "example-check".to_owned(),
            description: "An example check for smoke testing.".to_owned(),
            default_severity: types::Severity::Warning,
            access_scope: None,
        };
        assert_eq!(desc.name, "example-check");
        assert_eq!(desc.default_severity, types::Severity::Warning);
        assert!(desc.access_scope.is_none());
    }

    #[test]
    fn access_scope_variants_are_generated() {
        let _modified_only = types::AccessScope::ModifiedOnly;
        let _whole_repo = types::AccessScope::WholeRepo;
        let _globs = types::AccessScope::Globs(vec!["**/Cargo.toml".to_owned()]);
    }

    #[test]
    fn check_error_variants_are_generated() {
        let unknown = types::CheckError::UnknownCheck("no-such-check".to_owned());
        let failed = types::CheckError::Failed("something went wrong".to_owned());
        match unknown {
            types::CheckError::UnknownCheck(name) => assert_eq!(name, "no-such-check"),
            types::CheckError::Failed(_) => panic!("unexpected variant"),
        }
        match failed {
            types::CheckError::Failed(msg) => assert_eq!(msg, "something went wrong"),
            types::CheckError::UnknownCheck(_) => panic!("unexpected variant"),
        }
    }

    #[test]
    fn fix_error_variants_are_generated() {
        // Proves the host bindgen picked up the new `fix-error` variant from the
        // WIT (which it must, for `call_fix_check` to be callable).
        let unknown = types::FixError::UnknownCheck("no-such-check".to_owned());
        let failed = types::FixError::Failed("fixer blew up".to_owned());
        let not_fixable = types::FixError::NotFixable;
        match unknown {
            types::FixError::UnknownCheck(name) => assert_eq!(name, "no-such-check"),
            _ => panic!("unexpected variant"),
        }
        match failed {
            types::FixError::Failed(msg) => assert_eq!(msg, "fixer blew up"),
            _ => panic!("unexpected variant"),
        }
        assert!(matches!(not_fixable, types::FixError::NotFixable));
    }

    #[test]
    fn file_edit_can_be_constructed() {
        let edit = types::FileEdit {
            path: "src/lib.rs".to_owned(),
            old_text: "foo".to_owned(),
            new_text: "bar".to_owned(),
        };
        assert_eq!(edit.path, "src/lib.rs");
        assert_eq!(edit.new_text, "bar");
    }

    #[test]
    fn finding_can_be_constructed() {
        let finding = types::Finding {
            severity: types::Severity::Error,
            message: "something is wrong".to_owned(),
            location: Some(types::Location {
                path: "src/lib.rs".to_owned(),
                line: Some(42),
                column: None,
            }),
            remediations: vec!["fix it".to_owned()],
            suggested_fix: None,
        };
        assert_eq!(finding.severity, types::Severity::Error);
        assert_eq!(finding.location.as_ref().unwrap().line, Some(42));
    }
}
