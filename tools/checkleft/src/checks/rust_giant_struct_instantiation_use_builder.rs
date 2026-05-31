use anyhow::{Context, Result};
use async_trait::async_trait;
use globset::GlobSet;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::check::{Check, ConfiguredCheck};
use crate::exclusion::{DeclaredExclusion, ExclusionStatus};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

use super::rust_giant_struct_common::{
    DEFAULT_MAX_FIELDS, has_cfg_test, is_excluded, is_literal_path, parse_exclude_files,
    strip_visibility, struct_declaration_line,
};

const CHECK_ID: &str = "rust-giant-struct-instantiation-use-builder";

/// Flags call sites where a struct literal sets more than `max_fields` named fields
/// without a functional-update spread (`..rest`).
///
/// Counting the explicitly-set fields in the literal is a sound lower bound on struct
/// size: you cannot set fields that don't exist. No cross-file struct-definition lookup
/// is required; the literal itself is the evidence.
///
/// Spread / functional-update literals (`Foo { a, ..rest }`) are exempt: they absorb
/// new fields without requiring call-site edits, which is the churn the check exists
/// to prevent.
///
/// For one-off exceptions use:
///   BYPASS_RUST_GIANT_STRUCT_INSTANTIATION_USE_BUILDER=<specific reason>
/// in the PR or commit description (requires `allow_bypass = true` in policy).
pub struct RustGiantStructInstantiationUseBuilderCheck;

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    max_fields: Option<i64>,
    #[serde(default, alias = "exclude_globs")]
    exclude_files: Option<Vec<String>>,
    #[serde(default)]
    exclude_structs: Option<Vec<String>>,
}

struct ParsedConfig {
    max_fields: usize,
    exclude_files: Option<GlobSet>,
    /// Simple names scoped to the config subtree (see `config_dir`).
    exclude_structs: HashSet<String>,
    /// Qualified exclusions: repo-root-relative file path → set of exempt struct names.
    exclude_structs_qualified: HashMap<PathBuf, HashSet<String>>,
    /// Directory of the CHECKS.toml that owns this config (repo-root-relative).
    config_dir: Option<PathBuf>,
    /// Exclusion entries eligible for stale-exclusion auditing.
    auditable_exclusions: Vec<AuditableExclusion>,
}

struct AuditableExclusion {
    entry: String,
    path: PathBuf,
    kind: AuditKind,
}

enum AuditKind {
    /// `path::Struct` exclusion: stale once `Struct` is no longer giant in `path`.
    Struct(String),
    /// Literal-path `exclude_files` entry: stale once the file no longer exists.
    File,
}

#[async_trait]
impl Check for RustGiantStructInstantiationUseBuilderCheck {
    fn id(&self) -> &str {
        CHECK_ID
    }

    fn description(&self) -> &str {
        "flags call sites that construct a struct with more than the configured number of explicit named fields instead of a builder"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config, None)?))
    }

    fn configure_scoped(
        &self,
        config: &toml::Value,
        config_dir: Option<&Path>,
    ) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config, config_dir)?))
    }
}

#[async_trait]
impl ConfiguredCheck for ParsedConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();
        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !changed_file.path.extension().map_or(false, |e| e == "rs") {
                continue;
            }
            if let Some(globs) = &self.exclude_files {
                let config_dir = self.config_dir.as_deref().unwrap_or(Path::new(""));
                if is_excluded(&changed_file.path, globs, config_dir) {
                    continue;
                }
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(source) = std::str::from_utf8(&contents) else {
                continue;
            };
            let Ok(parsed_file) = syn::parse_file(source) else {
                continue;
            };

            // Build effective exclusion set for this file.
            let in_scope = self
                .config_dir
                .as_ref()
                .map_or(true, |dir| changed_file.path.starts_with(dir));
            let mut effective_exclude: HashSet<String> = if in_scope {
                self.exclude_structs.clone()
            } else {
                HashSet::new()
            };
            if let Some(per_file) = self.exclude_structs_qualified.get(&changed_file.path) {
                effective_exclude.extend(per_file.iter().cloned());
            }

            // Collect large struct literals (name, explicit field count) from this file.
            let mut large_literals: Vec<(String, usize)> = Vec::new();
            collect_large_literals_in_items(
                &parsed_file.items,
                false,
                self.max_fields,
                &mut large_literals,
            );

            // Emit one finding per unique struct name (all literal sites for that name).
            let mut seen: HashSet<String> = HashSet::new();
            for (name, count) in &large_literals {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if effective_exclude.contains(name) {
                    continue;
                }

                let message = format!(
                    "struct `{name}` is constructed with {count} explicit fields — \
                     use a builder (`{name}::builder()…build()`); \
                     add a builder derive if it doesn't have one"
                );
                let remediation = Some(format!(
                    "Replace this struct literal with a builder call: \
                     `{name}::builder()…build()`.\n\
                     Add `#[derive(bon::Builder)]` (and `#[builder(on(String, into))]` per \
                     the project convention) if the struct does not already have a builder.\n\
                     Permanently exempt a struct by adding it to `exclude_structs` in `CHECKS.toml`."
                ));

                let lines = find_struct_literal_lines(source, name);
                for line in lines {
                    findings.push(Finding {
                        severity: Severity::Error,
                        message: message.clone(),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: Some(line),
                            column: Some(1),
                        }),
                        remediation: remediation.clone(),
                        suggested_fix: None,
                    });
                }
            }
        }

        Ok(CheckResult {
            check_id: CHECK_ID.to_owned(),
            findings,
        })
    }

    fn declared_exclusions(&self) -> Vec<DeclaredExclusion> {
        self.auditable_exclusions
            .iter()
            .map(|excl| DeclaredExclusion::new(excl.entry.clone(), vec![excl.path.clone()]))
            .collect()
    }

    async fn evaluate_exclusion(
        &self,
        exclusion: &DeclaredExclusion,
        tree: &dyn SourceTree,
    ) -> Result<ExclusionStatus> {
        let Some(spec) = self
            .auditable_exclusions
            .iter()
            .find(|c| c.entry == exclusion.entry)
        else {
            return Ok(ExclusionStatus::Unknown);
        };

        match &spec.kind {
            AuditKind::File => {
                if tree.exists(&spec.path) {
                    Ok(ExclusionStatus::LoadBearing)
                } else {
                    Ok(ExclusionStatus::Stale {
                        reason: format!(
                            "the excluded file `{}` no longer exists",
                            spec.path.display()
                        ),
                    })
                }
            }
            AuditKind::Struct(name) => {
                if !tree.exists(&spec.path) {
                    return Ok(ExclusionStatus::Stale {
                        reason: format!("the file `{}` no longer exists", spec.path.display()),
                    });
                }
                let Ok(contents) = tree.read_file(&spec.path) else {
                    return Ok(ExclusionStatus::Unknown);
                };
                let Ok(source) = std::str::from_utf8(&contents) else {
                    return Ok(ExclusionStatus::Unknown);
                };
                let Ok(parsed_file) = syn::parse_file(source) else {
                    return Ok(ExclusionStatus::Unknown);
                };

                // Exclusion is load-bearing while the struct is still giant.
                if struct_is_giant(&parsed_file.items, name, self.max_fields, false) {
                    return Ok(ExclusionStatus::LoadBearing);
                }

                let reason = if struct_declaration_line(source, name).is_some() {
                    format!("`{name}` is no longer giant (below the field threshold)")
                } else {
                    format!("`{name}` is no longer defined in `{}`", spec.path.display())
                };
                Ok(ExclusionStatus::Stale { reason })
            }
        }
    }
}

/// Returns true if any named struct called `name` in `items` (or nested non-test mods)
/// has more than `max_fields` named fields and is not in a `#[cfg(test)]` context.
fn struct_is_giant(
    items: &[syn::Item],
    name: &str,
    max_fields: usize,
    in_test_mod: bool,
) -> bool {
    for item in items {
        match item {
            syn::Item::Struct(s) if !in_test_mod && !has_cfg_test(&s.attrs) => {
                if s.ident == name {
                    if let syn::Fields::Named(named) = &s.fields {
                        if named.named.len() > max_fields {
                            return true;
                        }
                    }
                }
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    if struct_is_giant(sub_items, name, max_fields, in_test_mod || is_test) {
                        return true;
                    }
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
                    if let syn::TraitItem::Fn(f) = trait_item {
                        if let Some(body) = &f.default {
                            if !in_test_mod {
                                collect_large_in_block(body, max_fields, out);
                            }
                        }
                    }
                }
            }
            syn::Item::Const(c) => {
                if !in_test_mod {
                    collect_large_in_expr(&c.expr, max_fields, out);
                }
            }
            syn::Item::Static(s) => {
                if !in_test_mod {
                    collect_large_in_expr(&s.expr, max_fields, out);
                }
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    collect_large_literals_in_items(
                        sub_items,
                        in_test_mod || is_test,
                        max_fields,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

fn collect_large_in_block(
    block: &syn::Block,
    max_fields: usize,
    out: &mut Vec<(String, usize)>,
) {
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
                collect_large_literals_in_items(
                    std::slice::from_ref(item),
                    false,
                    max_fields,
                    out,
                );
            }
            syn::Stmt::Expr(e, _) => collect_large_in_expr(e, max_fields, out),
            syn::Stmt::Macro(_) => {}
        }
    }
}

fn collect_large_in_expr(
    expr: &syn::Expr,
    max_fields: usize,
    out: &mut Vec<(String, usize)>,
) {
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
    if lines.is_empty() {
        vec![1]
    } else {
        lines
    }
}

fn parse_config(config: &toml::Value, config_dir: Option<&Path>) -> Result<ParsedConfig> {
    let raw: RawConfig = config
        .clone()
        .try_into()
        .context("invalid rust-giant-struct-instantiation-use-builder config")?;

    let max_fields = match raw.max_fields {
        Some(v) => usize::try_from(v).context("`max_fields` must be a non-negative integer")?,
        None => DEFAULT_MAX_FIELDS,
    };

    let mut exclude_structs: HashSet<String> = HashSet::new();
    let mut exclude_structs_qualified: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut auditable_exclusions: Vec<AuditableExclusion> = Vec::new();

    let resolve = |path_part: &str| -> PathBuf {
        match config_dir {
            Some(dir) => dir.join(path_part),
            None => PathBuf::from(path_part),
        }
    };

    let raw_exclude_files = raw.exclude_files.clone().unwrap_or_default();
    for entry in raw.exclude_structs.unwrap_or_default() {
        if let Some(sep) = entry.rfind("::") {
            let path_part = &entry[..sep];
            let name_part = entry[sep + 2..].to_owned();
            let resolved = resolve(path_part);
            auditable_exclusions.push(AuditableExclusion {
                entry: entry.clone(),
                path: resolved.clone(),
                kind: AuditKind::Struct(name_part.clone()),
            });
            exclude_structs_qualified
                .entry(resolved)
                .or_default()
                .insert(name_part);
        } else {
            exclude_structs.insert(entry);
        }
    }

    for pattern in &raw_exclude_files {
        if is_literal_path(pattern) {
            auditable_exclusions.push(AuditableExclusion {
                entry: pattern.clone(),
                path: resolve(pattern),
                kind: AuditKind::File,
            });
        }
    }

    Ok(ParsedConfig {
        max_fields,
        exclude_files: parse_exclude_files(raw.exclude_files.as_deref())?,
        exclude_structs,
        exclude_structs_qualified,
        config_dir: config_dir.map(|p| p.to_path_buf()),
        auditable_exclusions,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::{Check, ConfiguredCheck};
    use crate::exclusion::ExclusionStatus;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Finding;
    use crate::source_tree::LocalSourceTree;

    use super::RustGiantStructInstantiationUseBuilderCheck;

    async fn run_check_findings(source: &str, config: toml::Value) -> Vec<Finding> {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("test.rs"), source).expect("write file");
        let check = RustGiantStructInstantiationUseBuilderCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("test.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        check
            .run(&changeset, &tree, &config)
            .await
            .expect("run check")
            .findings
    }

    async fn run_check(source: &str, config: toml::Value) -> Vec<String> {
        run_check_findings(source, config)
            .await
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    // -------------------------------------------------------------------------
    // Core: field-count-in-literal is the evidence
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn flags_literal_with_many_explicit_fields() {
        // Struct definition is absent — the literal alone is the evidence.
        let source = r#"
fn make() -> Big {
    Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert_eq!(messages.len(), 1, "expected one finding, got {messages:?}");
        assert!(messages[0].contains("Big"), "{}", messages[0]);
        assert!(messages[0].contains("6"), "{}", messages[0]);
        assert!(messages[0].contains("builder"), "{}", messages[0]);
    }

    #[tokio::test]
    async fn does_not_flag_literal_at_or_under_threshold() {
        let source = r#"
fn make() -> Small {
    Small { a: String::new(), b: String::new(), c: String::new() }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(messages.is_empty(), "small literal should not be flagged, got {messages:?}");
    }

    // -------------------------------------------------------------------------
    // Spread / functional-update literals are exempt
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn does_not_flag_spread_literal_with_few_explicit_fields() {
        // Even if Big has 6 fields, a spread literal that sets only 2 explicitly is exempt.
        let source = r#"
fn update(base: Big) -> Big {
    Big { a: String::new(), b: String::new(), ..base }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(
            messages.is_empty(),
            "spread literal with few explicit fields should be exempt, got {messages:?}"
        );
    }

    #[tokio::test]
    async fn does_not_flag_spread_literal_even_with_many_explicit_fields() {
        // A spread literal with many explicit fields is still exempt — it already absorbs
        // future field additions via the spread.
        let source = r#"
fn update(base: Big) -> Big {
    Big { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, ..base }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(
            messages.is_empty(),
            "spread literal should always be exempt, got {messages:?}"
        );
    }

    // -------------------------------------------------------------------------
    // Builder call site is not flagged (not an Expr::Struct)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn does_not_flag_builder_call_site() {
        let source = r#"
fn make() -> Big {
    Big::builder()
        .a(String::new())
        .b(String::new())
        .c(String::new())
        .d(String::new())
        .e(String::new())
        .f(String::new())
        .build()
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(
            messages.is_empty(),
            "builder call site should not be flagged, got {messages:?}"
        );
    }

    // -------------------------------------------------------------------------
    // Exemptions: #[cfg(test)], exclude_structs, exclude_files
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn does_not_flag_literal_in_cfg_test_module() {
        let source = r#"
#[cfg(test)]
mod tests {
    use super::Big;
    fn helper() -> Big {
        Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
    }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(
            messages.is_empty(),
            "struct literal in #[cfg(test)] module should be exempt, got {messages:?}"
        );
    }

    #[tokio::test]
    async fn does_not_flag_literal_in_cfg_test_fn() {
        let source = r#"
#[cfg(test)]
fn make_test_big() -> Big {
    Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() }
}
"#;
        let messages = run_check(source, toml::Value::Table(toml::Table::new())).await;
        assert!(
            messages.is_empty(),
            "struct literal in #[cfg(test)] fn should be exempt, got {messages:?}"
        );
    }

    #[tokio::test]
    async fn exclude_files_skips_changed_file() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("gen.rs"),
            r#"
fn make() -> Big { Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() } }
"#,
        )
        .expect("write file");
        let check = RustGiantStructInstantiationUseBuilderCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("gen.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        let messages: Vec<String> = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! { exclude_files = ["gen.rs"] }),
            )
            .await
            .expect("run check")
            .findings
            .into_iter()
            .map(|f| f.message)
            .collect();
        assert!(messages.is_empty(), "excluded file should not be flagged, got {messages:?}");
    }

    #[tokio::test]
    async fn exclude_structs_exempts_struct() {
        let source = r#"
fn make() -> Big { Big { a: String::new(), b: String::new(), c: String::new(), d: String::new(), e: String::new(), f: String::new() } }
"#;
        let messages = run_check(
            source,
            toml::Value::Table(toml::toml! { exclude_structs = ["Big"] }),
        )
        .await;
        assert!(messages.is_empty(), "excluded struct should not be flagged, got {messages:?}");
    }

    // -------------------------------------------------------------------------
    // Respects max_fields config
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn respects_custom_max_fields() {
        let source = r#"
fn make() -> Medium {
    Medium { a: String::new(), b: String::new(), c: String::new() }
}
"#;
        let config = toml::Value::Table(toml::toml! { max_fields = 2 });
        let messages = run_check(source, config).await;
        assert_eq!(messages.len(), 1, "expected one finding with max_fields=2, got {messages:?}");
        assert!(messages[0].contains("Medium"), "{}", messages[0]);
    }

    // -------------------------------------------------------------------------
    // Stale-exclusion auditing
    // -------------------------------------------------------------------------

    fn configured(
        config: toml::Value,
        config_dir: Option<&Path>,
    ) -> std::sync::Arc<dyn ConfiguredCheck> {
        RustGiantStructInstantiationUseBuilderCheck
            .configure_scoped(&config, config_dir)
            .expect("configure_scoped")
    }

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

    #[tokio::test]
    async fn exclusion_load_bearing_while_struct_still_giant() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("types.rs"), GIANT_STRUCT_SOURCE).expect("write file");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let check = configured(
            toml::Value::Table(toml::toml! { exclude_structs = ["types.rs::Big"] }),
            Some(Path::new("")),
        );
        let exclusion =
            crate::exclusion::DeclaredExclusion::new("types.rs::Big", vec!["types.rs".into()]);
        let status = check.evaluate_exclusion(&exclusion, &tree).await.expect("evaluate");
        assert_eq!(status, ExclusionStatus::LoadBearing);
    }

    #[tokio::test]
    async fn exclusion_stale_when_struct_shrank_below_threshold() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("types.rs"), SMALL_STRUCT_SOURCE).expect("write file");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let check = configured(
            toml::Value::Table(toml::toml! { exclude_structs = ["types.rs::Big"] }),
            Some(Path::new("")),
        );
        let exclusion =
            crate::exclusion::DeclaredExclusion::new("types.rs::Big", vec!["types.rs".into()]);
        let status = check.evaluate_exclusion(&exclusion, &tree).await.expect("evaluate");
        match status {
            ExclusionStatus::Stale { reason } => {
                assert!(
                    reason.contains("no longer giant") || reason.contains("no longer defined"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exclusion_stale_when_struct_file_deleted() {
        let temp = tempdir().expect("create temp dir");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let check = configured(
            toml::Value::Table(toml::toml! { exclude_structs = ["types.rs::Big"] }),
            Some(Path::new("")),
        );
        let exclusion =
            crate::exclusion::DeclaredExclusion::new("types.rs::Big", vec!["types.rs".into()]);
        let status = check.evaluate_exclusion(&exclusion, &tree).await.expect("evaluate");
        match status {
            ExclusionStatus::Stale { reason } => {
                assert!(reason.contains("no longer exists"), "unexpected reason: {reason}");
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }
}
