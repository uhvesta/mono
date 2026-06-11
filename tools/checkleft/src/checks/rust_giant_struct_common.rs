use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::Path;

pub const DEFAULT_MAX_FIELDS: usize = 5;

#[derive(Clone, Debug)]
pub enum BuilderKind {
    Bon,
    DeriveBuilder,
}

impl BuilderKind {
    pub fn derive_display(&self) -> &str {
        match self {
            Self::Bon => "bon::Builder",
            Self::DeriveBuilder => "derive_builder::Builder",
        }
    }

    pub fn crate_name(&self) -> &str {
        match self {
            Self::Bon => "bon",
            Self::DeriveBuilder => "derive_builder",
        }
    }
}

/// Whether a giant struct has a builder derive or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GiantStructKind {
    WithBuilder,
    WithoutBuilder,
}

/// A giant struct found in a source file.
pub struct GiantStructInfo {
    pub name: String,
    pub kind: GiantStructKind,
}

pub fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().ok().is_some_and(|id| id == "test"))
}

/// Returns true if the struct carries a clap argument-parser derive.
/// Such structs are exempt because clap owns their construction via its derive.
pub fn has_clap_derive(attrs: &[syn::Attribute]) -> bool {
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

pub fn has_required_builder(attrs: &[syn::Attribute], builder: &BuilderKind) -> bool {
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
            match builder {
                BuilderKind::Bon => {
                    if segs.len() == 2 && segs[0].ident == "bon" && segs[1].ident == "Builder" {
                        return true;
                    }
                }
                BuilderKind::DeriveBuilder => {
                    if segs.len() == 2 && segs[0].ident == "derive_builder" && segs[1].ident == "Builder" {
                        return true;
                    }
                    // Unqualified Builder is also accepted for derive_builder
                    if segs.len() == 1 && segs[0].ident == "Builder" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Recursively scan `items` for giant structs, returning info about each one found.
/// Skips: test-cfg structs/modules, clap-derived structs, tuple/unit structs.
/// Does NOT apply `exclude_structs`; the caller filters based on context.
pub fn collect_giant_struct_infos(
    items: &[syn::Item],
    in_test_mod: bool,
    builder: &BuilderKind,
    max_fields: usize,
) -> Vec<GiantStructInfo> {
    let mut infos = Vec::new();
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
                let kind = if has_required_builder(&s.attrs, builder) {
                    GiantStructKind::WithBuilder
                } else {
                    GiantStructKind::WithoutBuilder
                };
                infos.push(GiantStructInfo {
                    name: s.ident.to_string(),
                    kind,
                });
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    infos.extend(collect_giant_struct_infos(
                        sub_items,
                        in_test_mod || is_test,
                        builder,
                        max_fields,
                    ));
                }
            }
            _ => {}
        }
    }
    infos
}

/// Recursively walk `items` collecting the names of giant structs that VIOLATE the rule
/// (giant + no required builder + not in `exclude_structs`).
/// This is a thin filter over [`collect_giant_struct_infos`].
pub fn collect_violations(
    items: &[syn::Item],
    in_test_mod: bool,
    builder: &BuilderKind,
    max_fields: usize,
    exclude_structs: &HashSet<String>,
) -> Vec<String> {
    collect_giant_struct_infos(items, in_test_mod, builder, max_fields)
        .into_iter()
        .filter(|info| !matches!(info.kind, GiantStructKind::WithBuilder) && !exclude_structs.contains(&info.name))
        .map(|info| info.name)
        .collect()
}

/// Find the 1-based line number where `struct <name>` is declared, or `None`.
/// Handles a leading visibility modifier (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`).
pub fn struct_declaration_line(source: &str, struct_name: &str) -> Option<u32> {
    let search = format!("struct {struct_name}");
    for (i, line) in source.lines().enumerate() {
        let candidate = strip_visibility(line.trim_start());
        if let Some(after) = candidate.strip_prefix(&search)
            && (after.is_empty() || matches!(after.chars().next(), Some(' ' | '\t' | '<' | '{' | '(')))
        {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Like [`struct_declaration_line`] but falls back to line 1 when the declaration can't
/// be located, for use as a finding location.
pub fn find_struct_line(source: &str, struct_name: &str) -> u32 {
    struct_declaration_line(source, struct_name).unwrap_or(1)
}

/// Strip a leading `pub` / `pub(...)` visibility modifier (and following whitespace) from
/// an already-`trim_start`ed line. Leaves the line untouched when there is no visibility
/// keyword (so `published` is not mistaken for `pub`).
pub fn strip_visibility(line: &str) -> &str {
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

/// A pattern with no glob metacharacters resolves to a single concrete file path.
pub fn is_literal_path(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']', '{', '}', '!'])
}

pub fn parse_exclude_files(patterns: Option<&[String]>) -> Result<Option<GlobSet>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid `exclude_files` pattern: {pattern}"))?;
        builder.add(glob);
    }
    Ok(Some(
        builder.build().context("failed to compile `exclude_files` patterns")?,
    ))
}

/// Returns true if `path` is within `config_dir` and matches `globs` (relative to config_dir).
/// Files outside the config_dir subtree are never excluded.
pub fn is_excluded(path: &Path, globs: &GlobSet, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    globs.is_match(relative)
}
