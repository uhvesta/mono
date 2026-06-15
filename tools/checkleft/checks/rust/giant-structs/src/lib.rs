//! Checkleft check: flag Rust structs with more named fields than `max_fields`
//! (default 5, meaning 6+) that do not carry the required builder derive.
//!
//! This is the Component Model wasm port of the built-in
//! `rust/giant-structs` check. It is authored on the guest SDK so
//! it runs inside the checkleft wasm host (T3-T6), reads files via the WASI
//! filesystem sandbox (T4), and is the acceptance proof for the CM-wasm project
//! (T10).
//!
//! ## What the check detects
//!
//! Tuple structs and unit structs are never flagged. Structs inside `#[cfg(test)]`
//! modules or decorated with `#[cfg(test)]` themselves are exempt. Structs that
//! `#[derive]` a clap argument-parser (`Parser`, `Args`, `Subcommand`) are also
//! exempt because clap owns their construction via its derive macro.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "max_fields": 5,        // threshold: structs with > max_fields fields are flagged
//!   "builder": "bon"        // "bon" (default) or "derive_builder"
//! }
//! ```

use checkleft_check_sdk::{ChangeKind, CheckInput, DeclaredExclusion, ExclusionStatus, Finding, check};
use serde::Deserialize;

const DEFAULT_MAX_FIELDS: usize = 5;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    max_fields: Option<usize>,
    /// "bon" (default) or "derive_builder"
    #[serde(default)]
    builder: Option<String>,
    /// Entries like "path/to/file.rs::StructName" that are exempt from the check.
    #[serde(default)]
    exclude_structs: Option<Vec<String>>,
}

#[check(
    name = "rust/giant-structs",
    description = "flags Rust structs with more than the configured number of named fields that lack a builder derive",
    severity = error
)]
pub fn giant_structs_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let max_fields = cfg.max_fields.unwrap_or(DEFAULT_MAX_FIELDS);
    let use_bon = cfg.builder.as_deref().unwrap_or("bon") != "derive_builder";
    let (builder_derive, builder_crate) = if use_bon {
        ("bon::Builder", "bon")
    } else {
        ("derive_builder::Builder", "derive_builder")
    };

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        if file.kind == ChangeKind::Deleted {
            continue;
        }
        if !file.path.ends_with(".rs") {
            continue;
        }

        let source = match std::fs::read_to_string(&file.path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let parsed = match syn::parse_file(&source) {
            Ok(f) => f,
            Err(_) => continue,
        };

        for struct_name in collect_violations(&parsed.items, false, use_bon, max_fields) {
            let line = find_struct_line(&source, &struct_name);
            findings.push(
                Finding::error(format!(
                    "struct `{struct_name}` has more than {max_fields} named fields but lacks `#[derive({builder_derive})]`"
                ))
                .at(&file.path, line)
                .with_remediation(format!(
                    "Add `#[derive({builder_crate}::Builder)]` (and `#[builder(on(String, into))]` \
                     per the project convention) above the struct."
                ))
                .with_remediation(
                    "Permanently exempt a file by adding it to `exclude_files` in the `CHECKS` file."
                        .to_owned(),
                ),
            );
        }
    }

    findings
}

pub fn giant_structs_declared_exclusions(config_json: &str) -> Vec<DeclaredExclusion> {
    let cfg: Config = serde_json::from_str(config_json).unwrap_or_default();
    let mut result = Vec::new();
    for entry in cfg.exclude_structs.unwrap_or_default() {
        if let Some((path_part, _struct_name)) = entry.split_once("::") {
            result.push(DeclaredExclusion {
                entry: entry.clone(),
                depends_on: vec![path_part.to_owned()],
            });
        }
    }
    result
}

pub fn giant_structs_evaluate_exclusion(
    config_json: &str,
    excl: &DeclaredExclusion,
    file_content: Option<&str>,
) -> ExclusionStatus {
    let cfg: Config = serde_json::from_str(config_json).unwrap_or_default();
    let max_fields = cfg.max_fields.unwrap_or(DEFAULT_MAX_FIELDS);
    let use_bon = cfg.builder.as_deref().unwrap_or("bon") != "derive_builder";

    let Some((_path_part, struct_name)) = excl.entry.split_once("::") else {
        return ExclusionStatus::Unknown;
    };

    let Some(source) = file_content else {
        return ExclusionStatus::Stale(format!("the file containing `{struct_name}` was deleted"));
    };

    let parsed = match syn::parse_file(source) {
        Ok(f) => f,
        Err(_) => return ExclusionStatus::Unknown,
    };

    if collect_violations(&parsed.items, false, use_bon, max_fields)
        .iter()
        .any(|name| name == struct_name)
    {
        ExclusionStatus::LoadBearing
    } else {
        ExclusionStatus::Stale(format!(
            "struct `{struct_name}` no longer violates the rule (it now has the required builder derive, or was removed)"
        ))
    }
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check` plus the `exclusion_audit`
// hooks for `rust/giant-structs`) is wired ONCE in the aggregating
// `checkleft-preinstalled-bundle` crate, which links this check and `file/size`
// into a single multiplexed component. That links `syn`, `serde`, and the wasm
// runtime baseline once across the preinstalled checks instead of per component.

// ── AST analysis ──────────────────────────────────────────────────────────────

/// Recursively walk `items` and return the name of each struct that violates
/// the rule (more than `max_fields` named fields, no required builder derive,
/// not in a test context, not clap-derived).
fn collect_violations(items: &[syn::Item], in_test_mod: bool, use_bon: bool, max_fields: usize) -> Vec<String> {
    let mut result = Vec::new();
    for item in items {
        match item {
            syn::Item::Struct(s) => {
                if in_test_mod || has_cfg_test(&s.attrs) {
                    continue;
                }
                let syn::Fields::Named(named) = &s.fields else {
                    continue;
                };
                if named.named.len() <= max_fields {
                    continue;
                }
                if has_clap_derive(&s.attrs) {
                    continue;
                }
                if has_required_builder(&s.attrs, use_bon) {
                    continue;
                }
                result.push(s.ident.to_string());
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    result.extend(collect_violations(
                        sub_items,
                        in_test_mod || is_test,
                        use_bon,
                        max_fields,
                    ));
                }
            }
            _ => {}
        }
    }
    result
}

/// Scan `source` for the 1-based line number where `struct <name>` is declared.
/// Falls back to line 1 when the declaration cannot be located (e.g. macro-generated).
fn find_struct_line(source: &str, struct_name: &str) -> u32 {
    let search = format!("struct {struct_name}");
    for (i, line) in source.lines().enumerate() {
        let candidate = strip_visibility(line.trim_start());
        if let Some(after) = candidate.strip_prefix(&search)
            && (after.is_empty() || matches!(after.chars().next(), Some(' ' | '\t' | '<' | '{' | '(')))
        {
            return (i + 1) as u32;
        }
    }
    1
}

/// Strip a leading `pub` / `pub(...)` visibility modifier from a trimmed line.
fn strip_visibility(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    match rest.chars().next() {
        Some('(') => match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => line,
        },
        Some(c) if c.is_whitespace() => rest.trim_start(),
        _ => line,
    }
}

fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().ok().is_some_and(|id| id == "test"))
}

fn has_clap_derive(attrs: &[syn::Attribute]) -> bool {
    const CLAP_TRAITS: &[&str] = &["Parser", "Args", "Subcommand"];
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(nested) =
            attr.parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
        else {
            continue;
        };
        for path in &nested {
            let segs: Vec<_> = path.segments.iter().collect();
            match segs.as_slice() {
                [seg] if CLAP_TRAITS.contains(&seg.ident.to_string().as_str()) => return true,
                [krate, trait_seg]
                    if krate.ident == "clap" && CLAP_TRAITS.contains(&trait_seg.ident.to_string().as_str()) =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

fn has_required_builder(attrs: &[syn::Attribute], use_bon: bool) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(nested) =
            attr.parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
        else {
            continue;
        };
        for path in &nested {
            let segs: Vec<_> = path.segments.iter().collect();
            if use_bon {
                if segs.len() == 2 && segs[0].ident == "bon" && segs[1].ident == "Builder" {
                    return true;
                }
            } else {
                if segs.len() == 2 && segs[0].ident == "derive_builder" && segs[1].ident == "Builder" {
                    return true;
                }
                if segs.len() == 1 && segs[0].ident == "Builder" {
                    return true;
                }
            }
        }
    }
    false
}

// ── Unit tests ──────────────────────────────────────────────────────────────────
//
// This is the layer-1 home for the giant-structs *behavioral matrix*: every
// detection rule is proven here against the pure check functions, with no wasm
// host involved (near-instant, no `.cwasm` compile/deserialize). The wasm
// boundary keeps only a thin round-trip set (see
// `checkleft_lib`'s `external::runtime::tests`) that proves the bundled
// component behaves identically to these native assertions.

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{
        ChangeKind, ChangeSet, ChangedFile, CheckInput, DeclaredExclusion, ExclusionStatus, Severity,
    };
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // A 6-named-field struct named `GiantStruct` with no builder derive — always a
    // violation at the default `max_fields = 5`.
    const VIOLATION: &str =
        "pub struct GiantStruct {\n    a: u8,\n    b: u8,\n    c: u8,\n    d: u8,\n    e: u8,\n    f: u8,\n}\n";

    // The same struct but with the required bon builder — no longer a violation.
    const VIOLATION_WITH_BUILDER: &str = "#[derive(bon::Builder)]\npub struct GiantStruct {\n    a: u8,\n    b: u8,\n    c: u8,\n    d: u8,\n    e: u8,\n    f: u8,\n}\n";

    // ── collect_violations: the AST behavioral matrix (no file IO) ────────────────

    fn violations(src: &str, use_bon: bool, max_fields: usize) -> Vec<String> {
        let parsed = syn::parse_file(src).expect("parse source");
        collect_violations(&parsed.items, false, use_bon, max_fields)
    }

    #[test]
    fn flags_struct_with_more_than_max_named_fields_and_no_builder() {
        assert_eq!(violations(VIOLATION, true, 5), vec!["GiantStruct".to_owned()]);
    }

    #[test]
    fn struct_at_or_below_max_fields_is_not_flagged() {
        let five = "pub struct Small { a: u8, b: u8, c: u8, d: u8, e: u8 }";
        assert!(violations(five, true, 5).is_empty());
    }

    #[test]
    fn struct_with_bon_builder_is_not_flagged() {
        assert!(violations(VIOLATION_WITH_BUILDER, true, 5).is_empty());
    }

    #[test]
    fn derive_builder_mode_accepts_qualified_and_bare_builder() {
        let qualified = "#[derive(derive_builder::Builder)]\npub struct A { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }";
        let bare = "#[derive(Builder)]\npub struct B { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }";
        assert!(violations(qualified, false, 5).is_empty());
        assert!(violations(bare, false, 5).is_empty());
        // In bon mode a bare `Builder` is NOT the required derive, so it still trips.
        assert_eq!(violations(bare, true, 5), vec!["B".to_owned()]);
    }

    #[test]
    fn tuple_and_unit_structs_are_never_flagged() {
        let src = "pub struct Tup(u8, u8, u8, u8, u8, u8, u8);\npub struct Unit;";
        assert!(violations(src, true, 5).is_empty());
    }

    #[test]
    fn cfg_test_struct_is_skipped() {
        let src = "#[cfg(test)]\npub struct OnlyInTests { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }";
        assert!(violations(src, true, 5).is_empty());
    }

    #[test]
    fn struct_inside_cfg_test_module_is_skipped() {
        let src = "#[cfg(test)]\nmod tests {\n    pub struct Helper { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\n}";
        assert!(violations(src, true, 5).is_empty());
    }

    #[test]
    fn clap_derives_are_exempt() {
        for derive in [
            "Parser",
            "Args",
            "Subcommand",
            "clap::Parser",
            "clap::Args",
            "clap::Subcommand",
        ] {
            let src = format!("#[derive({derive})]\npub struct Cli {{ a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }}");
            assert!(
                violations(&src, true, 5).is_empty(),
                "clap derive `{derive}` must be exempt"
            );
        }
    }

    #[test]
    fn max_fields_threshold_is_configurable() {
        let three = "pub struct S { a: u8, b: u8, c: u8 }";
        // 3 fields: flagged when max_fields = 2, allowed when max_fields = 3.
        assert_eq!(violations(three, true, 2), vec!["S".to_owned()]);
        assert!(violations(three, true, 3).is_empty());
    }

    #[test]
    fn nested_non_test_modules_are_recursed() {
        let src = "mod outer {\n    pub mod inner {\n        pub struct Deep { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\n    }\n}";
        assert_eq!(violations(src, true, 5), vec!["Deep".to_owned()]);
    }

    #[test]
    fn multiple_violations_are_all_reported() {
        let src = "pub struct One { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\npub struct Two { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }";
        assert_eq!(violations(src, true, 5), vec!["One".to_owned(), "Two".to_owned()]);
    }

    // ── giant_structs_check: file orchestration + finding shape ───────────────────
    //
    // The check reads changed files from the current working directory, so these
    // tests serialize on a CWD lock (mirroring the file/size crate's tests).

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn changeset_one(path: &str, kind: ChangeKind) -> ChangeSet {
        ChangeSet {
            changed_files: vec![ChangedFile {
                path: path.to_owned(),
                kind,
                old_path: None,
            }],
            file_diffs: vec![],
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
        }
    }

    fn run_check_in_dir(files: &[(&str, &str)], changeset: ChangeSet, config_json: &str) -> Vec<Finding> {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        for (rel, contents) in files {
            fs::write(dir.path().join(rel), contents).unwrap();
        }
        let input = CheckInput::__from_parts(changeset, config_json.to_owned());
        let findings = giant_structs_check(input);
        std::env::set_current_dir(old_cwd).unwrap();
        findings
    }

    #[test]
    fn check_flags_violation_with_message_remediation_and_location() {
        let findings = run_check_in_dir(
            &[("src.rs", VIOLATION)],
            changeset_one("src.rs", ChangeKind::Modified),
            "{}",
        );
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("GiantStruct"), "message: {}", finding.message);
        assert!(finding.message.contains("bon::Builder"), "message: {}", finding.message);
        assert!(finding.message.contains("more than 5"), "message: {}", finding.message);
        assert!(!finding.remediations.is_empty(), "must carry remediation hints");
        let loc = finding.location.as_ref().expect("finding has a location");
        assert_eq!(loc.path, "src.rs");
        assert_eq!(loc.line, Some(1));
    }

    #[test]
    fn check_uses_derive_builder_message_in_derive_builder_mode() {
        let findings = run_check_in_dir(
            &[("src.rs", VIOLATION)],
            changeset_one("src.rs", ChangeKind::Modified),
            r#"{"builder": "derive_builder"}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("derive_builder::Builder"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn check_respects_max_fields_config() {
        let three = "pub struct S {\n    a: u8,\n    b: u8,\n    c: u8,\n}\n";
        let findings = run_check_in_dir(
            &[("src.rs", three)],
            changeset_one("src.rs", ChangeKind::Modified),
            r#"{"max_fields": 2}"#,
        );
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("more than 2"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn check_skips_deleted_files() {
        let findings = run_check_in_dir(
            &[("src.rs", VIOLATION)],
            changeset_one("src.rs", ChangeKind::Deleted),
            "{}",
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn check_skips_non_rust_files() {
        let findings = run_check_in_dir(
            &[("notes.md", VIOLATION)],
            changeset_one("notes.md", ChangeKind::Modified),
            "{}",
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn check_ignores_unparseable_source() {
        let findings = run_check_in_dir(
            &[("bad.rs", "this is not valid rust ^^^")],
            changeset_one("bad.rs", ChangeKind::Modified),
            "{}",
        );
        assert!(findings.is_empty());
    }

    // ── giant_structs_declared_exclusions: exclusion-entry parsing ────────────────

    #[test]
    fn declared_exclusions_parses_qualified_entries_into_file_dependencies() {
        let declared = giant_structs_declared_exclusions(r#"{"exclude_structs": ["a/b.rs::Foo", "c.rs::Bar"]}"#);
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0].entry, "a/b.rs::Foo");
        assert_eq!(declared[0].depends_on, vec!["a/b.rs".to_owned()]);
        assert_eq!(declared[1].entry, "c.rs::Bar");
        assert_eq!(declared[1].depends_on, vec!["c.rs".to_owned()]);
    }

    #[test]
    fn declared_exclusions_ignores_unqualified_entries() {
        // A bare struct name (no `path::`) names no file, so it is never audited.
        assert!(giant_structs_declared_exclusions(r#"{"exclude_structs": ["JustAName"]}"#).is_empty());
    }

    #[test]
    fn declared_exclusions_empty_without_config() {
        assert!(giant_structs_declared_exclusions("{}").is_empty());
    }

    // ── giant_structs_evaluate_exclusion: stale vs load-bearing determination ─────

    fn excl(entry: &str) -> DeclaredExclusion {
        DeclaredExclusion {
            entry: entry.to_owned(),
            depends_on: vec![],
        }
    }

    #[test]
    fn evaluate_exclusion_load_bearing_when_struct_still_violates() {
        let status = giant_structs_evaluate_exclusion("{}", &excl("f.rs::GiantStruct"), Some(VIOLATION));
        assert!(matches!(status, ExclusionStatus::LoadBearing), "got {status:?}");
    }

    #[test]
    fn evaluate_exclusion_stale_when_struct_gains_builder() {
        let status = giant_structs_evaluate_exclusion("{}", &excl("f.rs::GiantStruct"), Some(VIOLATION_WITH_BUILDER));
        match status {
            ExclusionStatus::Stale(reason) => assert!(reason.contains("GiantStruct"), "reason: {reason}"),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_exclusion_stale_when_struct_removed() {
        let status = giant_structs_evaluate_exclusion("{}", &excl("f.rs::GiantStruct"), Some("pub struct Other;\n"));
        assert!(matches!(status, ExclusionStatus::Stale(_)), "got {status:?}");
    }

    #[test]
    fn evaluate_exclusion_stale_with_deleted_reason_when_file_is_gone() {
        let status = giant_structs_evaluate_exclusion("{}", &excl("f.rs::GiantStruct"), None);
        match status {
            ExclusionStatus::Stale(reason) => assert!(reason.contains("deleted"), "reason: {reason}"),
            other => panic!("expected Stale(deleted), got {other:?}"),
        }
    }

    #[test]
    fn evaluate_exclusion_unknown_for_unqualified_entry() {
        // Fail-safe: an entry we cannot pin to a struct name is never declared stale.
        let status = giant_structs_evaluate_exclusion("{}", &excl("NoColons"), Some(VIOLATION));
        assert!(matches!(status, ExclusionStatus::Unknown), "got {status:?}");
    }

    #[test]
    fn evaluate_exclusion_unknown_when_source_does_not_parse() {
        let status = giant_structs_evaluate_exclusion("{}", &excl("f.rs::GiantStruct"), Some("not valid rust ^^^"));
        assert!(matches!(status, ExclusionStatus::Unknown), "got {status:?}");
    }

    #[test]
    fn evaluate_exclusion_respects_max_fields_config() {
        // GiantStruct has 6 fields; with max_fields = 10 it no longer violates → stale.
        let status =
            giant_structs_evaluate_exclusion(r#"{"max_fields": 10}"#, &excl("f.rs::GiantStruct"), Some(VIOLATION));
        assert!(matches!(status, ExclusionStatus::Stale(_)), "got {status:?}");
    }
}
