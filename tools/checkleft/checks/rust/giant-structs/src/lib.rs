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

use checkleft_check_sdk::{ChangeKind, CheckInput, Finding, check, export_checks};
use serde::Deserialize;

const DEFAULT_MAX_FIELDS: usize = 5;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    max_fields: Option<usize>,
    /// "bon" (default) or "derive_builder"
    #[serde(default)]
    builder: Option<String>,
}

#[check(
    name = "rust/giant-structs",
    description = "flags Rust structs with more than the configured number of named fields that lack a builder derive",
    severity = error
)]
fn giant_structs_check(input: CheckInput) -> Vec<Finding> {
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

export_checks!(giant_structs_check);

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
