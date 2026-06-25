//! Checkleft check: flag struct INSTANTIATIONS (literals) that set more than
//! `max_fields` explicit named fields without a functional-update spread, and
//! should use a builder instead.
//!
//! Registered under the canonical id `rust/giant-structs-create`. Construction-site
//! counterpart of `rust/giant-structs` (which flags struct DEFINITIONS).
//!
//! ## What the check detects
//!
//! Counting the explicitly-set fields in a struct literal is a sound lower bound
//! on struct size: you cannot set fields that don't exist. No cross-file
//! struct-definition lookup is required; the literal itself is the evidence.
//!
//! Spread / functional-update literals (`Foo { a, ..rest }`) are exempt: they
//! absorb new fields without requiring call-site edits, which is the churn the
//! check exists to prevent. Literals inside `#[cfg(test)]` modules/functions are
//! also exempt.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "max_fields": 5,
//!   "exclude_structs": ["Big", "path/to/file.rs::Big"]
//! }
//! ```
//!
//! File exclusion (`exclude` / `exclude_files` / `exclude_globs`) is enforced by the
//! framework host, which subtracts excluded paths from the changeset before it is
//! lowered into this check — so an excluded file never reaches the loop below.
//!
//! `exclude_structs`: grandfathering exemptions. The guest emits a finding for
//! every violating struct (in repo-relative coordinates, with no knowledge of
//! `config_dir`); the HOST drops grandfathered ones (see `apply_struct_exclusions`
//! in `external/runtime.rs`). The guest only re-derives them here for the
//! stale-exclusion audit hooks below.
//!
//! For a one-off exception use `BYPASS_RUST_GIANT_STRUCTS_CREATE=<reason>` in the
//! PR or commit description (requires `allow_bypass = true` in policy).

use checkleft_check_sdk::{ChangeKind, CheckInput, DeclaredExclusion, ExclusionStatus, Finding, check};
use checkleft_rust_giant_structs_common::{has_cfg_test, strip_visibility, struct_declaration_line};
use serde::Deserialize;
use std::collections::HashSet;

const DEFAULT_MAX_FIELDS: usize = 5;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    max_fields: Option<usize>,
    #[serde(default)]
    exclude_structs: Option<Vec<String>>,
}

#[check(
    name = "rust/giant-structs-create",
    description = "flags call sites that construct a struct with more than the configured number of explicit named fields instead of a builder",
    severity = error,
    declared_exclusions = giant_structs_create_declared_exclusions,
    evaluate_exclusion = giant_structs_create_evaluate_exclusion
)]
pub fn giant_structs_create_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let max_fields = cfg.max_fields.unwrap_or(DEFAULT_MAX_FIELDS);

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

        // Collect large struct literals (name, explicit field count) from this file.
        let mut large_literals: Vec<(String, usize)> = Vec::new();
        collect_large_literals_in_items(&parsed.items, false, max_fields, &mut large_literals);

        // Emit one finding per unique struct name (all literal sites for that name).
        // `exclude_structs` is applied HOST-SIDE (the host knows `config_dir`), so
        // the guest does not filter it here.
        let mut seen: HashSet<String> = HashSet::new();
        for (name, count) in &large_literals {
            if !seen.insert(name.clone()) {
                continue;
            }

            let message = format!(
                "struct `{name}` is constructed with {count} explicit fields — \
                 use a builder (`{name}::builder()…build()`); \
                 add a builder derive if it doesn't have one"
            );

            for line in find_struct_literal_lines(&source, name) {
                findings.push(
                    Finding::error(message.clone())
                        .at_column(&file.path, line, 1)
                        .with_remediation(format!(
                            "Replace this struct literal with a builder call: \
                             `{name}::builder()…build()`."
                        ))
                        .with_remediation(
                            "Add `#[derive(bon::Builder)]` (and `#[builder(on(String, into))]` per \
                             the project convention) if the struct does not already have a builder."
                                .to_owned(),
                        )
                        .with_remediation(
                            "Permanently exempt a struct by adding it to `exclude_structs` in the `CHECKS` file."
                                .to_owned(),
                        ),
                );
            }
        }
    }

    findings
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check` plus the `exclusion_audit`
// hooks for `rust/giant-structs-create`) is wired ONCE in the aggregating
// `checkleft-preinstalled-bundle` crate, which links this check alongside
// `rust/giant-structs` and `file/size` into a single multiplexed component.

// ── Stale-exclusion audit hooks ────────────────────────────────────────────────

/// Declare the auditable exclusion entries for `rust/giant-structs-create`.
///
/// One entry form is auditable:
/// * `relative/path.rs::Name` — qualified struct exemption; stale once `Name` is
///   no longer giant in that file.
///
/// Simple `exclude_structs` names (no `::`) are NOT auditable: they depend on no
/// single file. File exclusion is host-owned (framework `exclude` key) and audited
/// separately, so this guest only declares `exclude_structs` entries. `depends_on`
/// paths are config-file-relative; the host resolves them to repo-root-relative
/// before re-evaluation.
pub fn giant_structs_create_declared_exclusions(config_json: &str) -> Vec<DeclaredExclusion> {
    let cfg: Config = serde_json::from_str(config_json).unwrap_or_default();
    let mut result = Vec::new();

    for entry in cfg.exclude_structs.unwrap_or_default() {
        if let Some((path_part, _name)) = entry.split_once("::") {
            result.push(DeclaredExclusion {
                entry: entry.clone(),
                depends_on: vec![path_part.to_owned()],
            });
        }
    }

    result
}

/// Re-evaluate a single exclusion as if it were absent. `file_content` is the
/// content of the first depended-on file (`None` when it was deleted/unreadable).
pub fn giant_structs_create_evaluate_exclusion(
    config_json: &str,
    excl: &DeclaredExclusion,
    file_content: Option<&str>,
) -> ExclusionStatus {
    let cfg: Config = serde_json::from_str(config_json).unwrap_or_default();
    let max_fields = cfg.max_fields.unwrap_or(DEFAULT_MAX_FIELDS);

    // Repo-relative path the host resolved from `depends_on` (for diagnostics).
    let dep_path = excl
        .depends_on
        .first()
        .map(String::as_str)
        .unwrap_or(excl.entry.as_str());

    match excl.entry.split_once("::") {
        // Qualified struct exemption: load-bearing while the struct is still giant.
        Some((_path_part, struct_name)) => {
            let Some(source) = file_content else {
                return ExclusionStatus::Stale(format!("the file `{dep_path}` no longer exists"));
            };
            let parsed = match syn::parse_file(source) {
                Ok(f) => f,
                Err(_) => return ExclusionStatus::Unknown,
            };
            if struct_is_giant(&parsed.items, struct_name, max_fields, false) {
                ExclusionStatus::LoadBearing
            } else if struct_declaration_line(source, struct_name).is_some() {
                ExclusionStatus::Stale(format!(
                    "`{struct_name}` is no longer giant (below the field threshold)"
                ))
            } else {
                ExclusionStatus::Stale(format!("`{struct_name}` is no longer defined in `{dep_path}`"))
            }
        }
        // Only qualified `path::Name` exemptions are auditable; anything else is not
        // declared, so fail safe rather than guess.
        None => ExclusionStatus::Unknown,
    }
}

// ── AST analysis ───────────────────────────────────────────────────────────────

/// Returns true if any named struct called `name` in `items` (or nested non-test
/// mods) has more than `max_fields` named fields and is not in a `#[cfg(test)]`
/// context.
fn struct_is_giant(items: &[syn::Item], name: &str, max_fields: usize, in_test_mod: bool) -> bool {
    for item in items {
        match item {
            syn::Item::Struct(s) if !in_test_mod && !has_cfg_test(&s.attrs) => {
                if s.ident == name
                    && let syn::Fields::Named(named) = &s.fields
                    && named.named.len() > max_fields
                {
                    return true;
                }
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content
                    && struct_is_giant(sub_items, name, max_fields, in_test_mod || is_test)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Collect `(name, explicit_field_count)` for every struct literal in `items` that:
/// - sets more than `max_fields` explicitly-named fields, AND
/// - has no functional-update spread (`..rest`).
///
/// Items inside `#[cfg(test)]` modules/functions are skipped.
fn collect_large_literals_in_items(
    items: &[syn::Item],
    in_test_mod: bool,
    max_fields: usize,
    out: &mut Vec<(String, usize)>,
) {
    for item in items {
        match item {
            syn::Item::Fn(f) => {
                if in_test_mod || has_cfg_test(&f.attrs) {
                    continue;
                }
                collect_large_in_block(&f.block, max_fields, out);
            }
            syn::Item::Impl(impl_block) => {
                for impl_item in &impl_block.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        if in_test_mod || has_cfg_test(&f.attrs) {
                            continue;
                        }
                        collect_large_in_block(&f.block, max_fields, out);
                    }
                }
            }
            syn::Item::Trait(t) => {
                for trait_item in &t.items {
                    if let syn::TraitItem::Fn(f) = trait_item
                        && let Some(body) = &f.default
                        && !in_test_mod
                    {
                        collect_large_in_block(body, max_fields, out);
                    }
                }
            }
            syn::Item::Const(c) if !in_test_mod => {
                collect_large_in_expr(&c.expr, max_fields, out);
            }
            syn::Item::Static(s) if !in_test_mod => {
                collect_large_in_expr(&s.expr, max_fields, out);
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    collect_large_literals_in_items(sub_items, in_test_mod || is_test, max_fields, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_large_in_block(block: &syn::Block, max_fields: usize, out: &mut Vec<(String, usize)>) {
    for stmt in &block.stmts {
        match stmt {
            syn::Stmt::Local(l) => {
                if let Some(init) = &l.init {
                    collect_large_in_expr(&init.expr, max_fields, out);
                    if let Some((_, diverge)) = &init.diverge {
                        collect_large_in_expr(diverge, max_fields, out);
                    }
                }
            }
            syn::Stmt::Item(item) => {
                collect_large_literals_in_items(std::slice::from_ref(item), false, max_fields, out);
            }
            syn::Stmt::Expr(e, _) => collect_large_in_expr(e, max_fields, out),
            syn::Stmt::Macro(_) => {}
        }
    }
}

fn collect_large_in_expr(expr: &syn::Expr, max_fields: usize, out: &mut Vec<(String, usize)>) {
    match expr {
        syn::Expr::Struct(s) => {
            let name = s
                .path
                .segments
                .last()
                .map(|seg| seg.ident.to_string())
                .unwrap_or_default();
            // Flag only if no spread and enough explicit fields.
            if !name.is_empty() && s.rest.is_none() && s.fields.len() > max_fields {
                out.push((name, s.fields.len()));
            }
            for field in &s.fields {
                collect_large_in_expr(&field.expr, max_fields, out);
            }
            if let Some(rest) = &s.rest {
                collect_large_in_expr(rest, max_fields, out);
            }
        }
        syn::Expr::Block(b) => collect_large_in_block(&b.block, max_fields, out),
        syn::Expr::Unsafe(u) => collect_large_in_block(&u.block, max_fields, out),
        syn::Expr::Const(c) => collect_large_in_block(&c.block, max_fields, out),
        syn::Expr::Call(c) => {
            collect_large_in_expr(&c.func, max_fields, out);
            for arg in &c.args {
                collect_large_in_expr(arg, max_fields, out);
            }
        }
        syn::Expr::MethodCall(m) => {
            collect_large_in_expr(&m.receiver, max_fields, out);
            for arg in &m.args {
                collect_large_in_expr(arg, max_fields, out);
            }
        }
        syn::Expr::Return(r) => {
            if let Some(e) = &r.expr {
                collect_large_in_expr(e, max_fields, out);
            }
        }
        syn::Expr::Match(m) => {
            collect_large_in_expr(&m.expr, max_fields, out);
            for arm in &m.arms {
                if let Some(g) = &arm.guard {
                    collect_large_in_expr(&g.1, max_fields, out);
                }
                collect_large_in_expr(&arm.body, max_fields, out);
            }
        }
        syn::Expr::If(i) => {
            collect_large_in_expr(&i.cond, max_fields, out);
            collect_large_in_block(&i.then_branch, max_fields, out);
            if let Some((_, e)) = &i.else_branch {
                collect_large_in_expr(e, max_fields, out);
            }
        }
        syn::Expr::While(w) => {
            collect_large_in_expr(&w.cond, max_fields, out);
            collect_large_in_block(&w.body, max_fields, out);
        }
        syn::Expr::ForLoop(f) => collect_large_in_block(&f.body, max_fields, out),
        syn::Expr::Loop(l) => collect_large_in_block(&l.body, max_fields, out),
        syn::Expr::Closure(c) => collect_large_in_expr(&c.body, max_fields, out),
        syn::Expr::Assign(a) => {
            collect_large_in_expr(&a.left, max_fields, out);
            collect_large_in_expr(&a.right, max_fields, out);
        }
        syn::Expr::Binary(b) => {
            collect_large_in_expr(&b.left, max_fields, out);
            collect_large_in_expr(&b.right, max_fields, out);
        }
        syn::Expr::Unary(u) => collect_large_in_expr(&u.expr, max_fields, out),
        syn::Expr::Reference(r) => collect_large_in_expr(&r.expr, max_fields, out),
        syn::Expr::Paren(p) => collect_large_in_expr(&p.expr, max_fields, out),
        syn::Expr::Tuple(t) => {
            for e in &t.elems {
                collect_large_in_expr(e, max_fields, out);
            }
        }
        syn::Expr::Array(a) => {
            for e in &a.elems {
                collect_large_in_expr(e, max_fields, out);
            }
        }
        syn::Expr::Repeat(r) => {
            collect_large_in_expr(&r.expr, max_fields, out);
            collect_large_in_expr(&r.len, max_fields, out);
        }
        syn::Expr::Index(i) => {
            collect_large_in_expr(&i.expr, max_fields, out);
            collect_large_in_expr(&i.index, max_fields, out);
        }
        syn::Expr::Field(f) => collect_large_in_expr(&f.base, max_fields, out),
        syn::Expr::Cast(c) => collect_large_in_expr(&c.expr, max_fields, out),
        syn::Expr::Await(a) => collect_large_in_expr(&a.base, max_fields, out),
        syn::Expr::Try(t) => collect_large_in_expr(&t.expr, max_fields, out),
        syn::Expr::Range(r) => {
            if let Some(start) = &r.start {
                collect_large_in_expr(start, max_fields, out);
            }
            if let Some(end) = &r.end {
                collect_large_in_expr(end, max_fields, out);
            }
        }
        syn::Expr::Let(l) => collect_large_in_expr(&l.expr, max_fields, out),
        _ => {}
    }
}

/// Find all 1-based line numbers in `source` where `struct_name` appears as a struct literal.
/// Falls back to `[1]` if nothing is found precisely (syn detected it but text search cannot
/// locate it).
fn find_struct_literal_lines(source: &str, struct_name: &str) -> Vec<u32> {
    let search_ws = format!("{struct_name} {{");
    let search_no_ws = format!("{struct_name}{{");

    let mut lines = Vec::new();
    for (i, line) in source.lines().enumerate() {
        if !line.contains(&search_ws) && !line.contains(&search_no_ws) {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        let stripped = strip_visibility(trimmed);
        if stripped.starts_with("struct ") || stripped.starts_with("impl ") {
            continue;
        }
        // Block-opener lines (fn, while, etc.) end with `{` as the trailing brace.
        if line.trim_end().ends_with('{') {
            continue;
        }
        lines.push((i + 1) as u32);
    }
    if lines.is_empty() { vec![1] } else { lines }
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeSet, ChangedFile};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Serialize CWD changes so parallel tests don't interfere.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn changeset_for(path: &str) -> ChangeSet {
        ChangeSet {
            changed_files: vec![ChangedFile {
                path: path.to_owned(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            file_diffs: vec![],
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
            base_files: vec![],
        }
    }

    /// Write `source` to `path` in a fresh tempdir, run the check from that CWD,
    /// and return the finding messages.
    fn run_check(path: &str, source: &str, config_json: &str) -> Vec<String> {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        fs::write(dir.path().join(path), source).unwrap();

        let input = CheckInput::__from_parts(changeset_for(path), config_json.to_owned());
        let findings = giant_structs_create_check(input);

        std::env::set_current_dir(old_cwd).unwrap();
        findings.into_iter().map(|f| f.message).collect()
    }

    // ── Core: field-count-in-literal is the evidence ───────────────────────────

    #[test]
    fn flags_literal_with_many_explicit_fields() {
        // Struct definition is absent — the literal alone is the evidence.
        let source = r#"
fn make() -> Big {
    Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert_eq!(messages.len(), 1, "expected one finding, got {messages:?}");
        assert!(messages[0].contains("Big"), "{}", messages[0]);
        assert!(messages[0].contains('6'), "{}", messages[0]);
        assert!(messages[0].contains("builder"), "{}", messages[0]);
    }

    #[test]
    fn does_not_flag_literal_at_or_under_threshold() {
        let source = r#"
fn make() -> Small {
    Small { a: String::new(), b: String::new(), c: String::new() }
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert!(
            messages.is_empty(),
            "small literal should not be flagged, got {messages:?}"
        );
    }

    // ── Spread / functional-update literals are exempt ─────────────────────────

    #[test]
    fn does_not_flag_spread_literal_even_with_many_explicit_fields() {
        let source = r#"
fn update(base: Big) -> Big {
    Big { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, ..base }
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert!(
            messages.is_empty(),
            "spread literal should always be exempt, got {messages:?}"
        );
    }

    // ── Builder call site is not flagged (not an Expr::Struct) ──────────────────

    #[test]
    fn does_not_flag_builder_call_site() {
        let source = r#"
fn make() -> Big {
    Big::builder().a(1).b(2).c(3).d(4).e(5).f(6).build()
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert!(
            messages.is_empty(),
            "builder call site should not be flagged, got {messages:?}"
        );
    }

    // ── Exemptions: #[cfg(test)] ───────────────────────────────────────────────

    #[test]
    fn does_not_flag_literal_in_cfg_test_module() {
        let source = r#"
#[cfg(test)]
mod tests {
    use super::Big;
    fn helper() -> Big {
        Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
    }
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert!(
            messages.is_empty(),
            "literal in #[cfg(test)] module should be exempt, got {messages:?}"
        );
    }

    #[test]
    fn does_not_flag_literal_in_cfg_test_fn() {
        let source = r#"
#[cfg(test)]
fn make_test_big() -> Big {
    Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
}
"#;
        let messages = run_check("test.rs", source, "{}");
        assert!(
            messages.is_empty(),
            "literal in #[cfg(test)] fn should be exempt, got {messages:?}"
        );
    }

    // ── Respects max_fields config ─────────────────────────────────────────────

    #[test]
    fn respects_custom_max_fields() {
        let source = r#"
fn make() -> Medium {
    Medium { a: String::new(), b: String::new(), c: String::new() }
}
"#;
        let messages = run_check("test.rs", source, r#"{"max_fields": 2}"#);
        assert_eq!(
            messages.len(),
            1,
            "expected one finding with max_fields=2, got {messages:?}"
        );
        assert!(messages[0].contains("Medium"), "{}", messages[0]);
    }

    // ── Stale-exclusion auditing ───────────────────────────────────────────────

    const GIANT_STRUCT_SOURCE: &str = r#"
pub struct Big {
    a: String, b: String, c: String, d: String, e: String, f: String,
}
"#;
    const SMALL_STRUCT_SOURCE: &str = r#"
pub struct Big {
    a: String,
}
"#;

    #[test]
    fn declares_only_qualified_struct_exclusions() {
        // `exclude_files` is now host-owned and ignored by this guest; only qualified
        // `path::Name` struct exemptions are auditable. The simple struct name is not.
        let config = r#"{"exclude_structs": ["types.rs::Big", "Simple"], "exclude_files": ["gen.rs", "**/*.rs"]}"#;
        let declared = giant_structs_create_declared_exclusions(config);
        let entries: Vec<&str> = declared.iter().map(|d| d.entry.as_str()).collect();
        assert!(entries.contains(&"types.rs::Big"), "{entries:?}");
        assert!(
            !entries.contains(&"Simple"),
            "simple name must not be auditable: {entries:?}"
        );
        assert!(
            !entries.contains(&"gen.rs"),
            "exclude_files entries must not be auditable by the guest: {entries:?}"
        );
        assert!(!entries.contains(&"**/*.rs"), "glob must not be auditable: {entries:?}");
    }

    #[test]
    fn exclusion_load_bearing_while_struct_still_giant() {
        let excl = DeclaredExclusion {
            entry: "types.rs::Big".to_owned(),
            depends_on: vec!["types.rs".to_owned()],
        };
        let status = giant_structs_create_evaluate_exclusion("{}", &excl, Some(GIANT_STRUCT_SOURCE));
        assert!(matches!(status, ExclusionStatus::LoadBearing), "got {status:?}");
    }

    #[test]
    fn exclusion_stale_when_struct_shrank_below_threshold() {
        let excl = DeclaredExclusion {
            entry: "types.rs::Big".to_owned(),
            depends_on: vec!["types.rs".to_owned()],
        };
        let status = giant_structs_create_evaluate_exclusion("{}", &excl, Some(SMALL_STRUCT_SOURCE));
        match status {
            ExclusionStatus::Stale(reason) => {
                assert!(
                    reason.contains("no longer giant") || reason.contains("no longer defined"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn exclusion_stale_when_struct_file_deleted() {
        let excl = DeclaredExclusion {
            entry: "types.rs::Big".to_owned(),
            depends_on: vec!["types.rs".to_owned()],
        };
        let status = giant_structs_create_evaluate_exclusion("{}", &excl, None);
        match status {
            ExclusionStatus::Stale(reason) => assert!(reason.contains("no longer exists"), "reason: {reason}"),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn non_qualified_entry_is_not_audited() {
        // A bare `exclude_files`-style entry (no `::`) is no longer auditable by the
        // guest: it fails safe to Unknown rather than guessing staleness.
        let excl = DeclaredExclusion {
            entry: "gen.rs".to_owned(),
            depends_on: vec!["gen.rs".to_owned()],
        };
        assert!(matches!(
            giant_structs_create_evaluate_exclusion("{}", &excl, Some("fn x() {}")),
            ExclusionStatus::Unknown
        ));
        assert!(matches!(
            giant_structs_create_evaluate_exclusion("{}", &excl, None),
            ExclusionStatus::Unknown
        ));
    }
}
