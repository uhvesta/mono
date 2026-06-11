//! Proc-macro support for `checkleft-check-sdk`.
//!
//! Provides the `#[check]` attribute macro for annotating check functions and
//! the `export_checks!` function-like macro for wiring the guest component.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprArray, ExprLit, Ident, ItemFn, Lit, LitStr, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
};

// The WIT contract embedded at proc-macro compile time so that export_checks!
// can emit a wit_bindgen::generate! call with the exact WIT version that
// matches this SDK. The path is relative to this source file.
const WIT_CONTENT: &str = include_str!("../../wit/check.wit");

// ---------------------------------------------------------------------------
// #[check] attribute macro
// ---------------------------------------------------------------------------

struct CheckArgs {
    name: String,
    description: Option<String>,
    severity: SeverityArg,
    access_scope: AccessScopeArg,
}

#[derive(Default)]
enum SeverityArg {
    Error,
    #[default]
    Warning,
    Info,
}

#[derive(Default)]
enum AccessScopeArg {
    #[default]
    ModifiedOnly,
    WholeRepo,
    Globs(Vec<String>),
}

impl Parse for CheckArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name: Option<String> = None;
        let mut description: Option<String> = None;
        let mut severity = SeverityArg::Warning;
        let mut access_scope = AccessScopeArg::ModifiedOnly;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "name" => {
                    let lit: LitStr = input.parse()?;
                    name = Some(lit.value());
                }
                "description" => {
                    let lit: LitStr = input.parse()?;
                    description = Some(lit.value());
                }
                "severity" => {
                    let ident: Ident = input.parse()?;
                    severity = match ident.to_string().as_str() {
                        "error" => SeverityArg::Error,
                        "warning" => SeverityArg::Warning,
                        "info" => SeverityArg::Info,
                        other => {
                            return Err(syn::Error::new(
                                ident.span(),
                                format!("unknown severity `{other}`: expected `error`, `warning`, or `info`"),
                            ));
                        }
                    };
                }
                "access_scope" => {
                    access_scope = parse_access_scope(input)?;
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown #[check] argument `{other}`"),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let name = name.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[check] requires `name = \"...\"` argument",
            )
        })?;

        Ok(CheckArgs {
            name,
            description,
            severity,
            access_scope,
        })
    }
}

fn parse_access_scope(input: ParseStream<'_>) -> syn::Result<AccessScopeArg> {
    let ident: Ident = input.parse()?;
    match ident.to_string().as_str() {
        "modified_only" => Ok(AccessScopeArg::ModifiedOnly),
        "whole_repo" => Ok(AccessScopeArg::WholeRepo),
        "globs" => {
            let content;
            syn::parenthesized!(content in input);
            let arr: ExprArray = content.parse()?;
            let mut patterns = Vec::new();
            for elem in &arr.elems {
                if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = elem {
                    patterns.push(s.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        elem,
                        "expected string literal in globs([...]) patterns",
                    ));
                }
            }
            Ok(AccessScopeArg::Globs(patterns))
        }
        other => Err(syn::Error::new(
            ident.span(),
            format!("unknown access_scope `{other}`: expected `modified_only`, `whole_repo`, or `globs([...])`"),
        )),
    }
}

/// Annotate a function as a checkleft check.
///
/// # Arguments
/// - `name = "..."` (required): The check name used for dispatch.
/// - `description = "..."` (optional): Human-readable description; defaults to name.
/// - `severity = warning|error|info` (optional): Default severity (default: `warning`).
/// - `access_scope = modified_only|whole_repo|globs([...])` (optional): File-access
///   scope (default: `modified_only`).
///
/// After annotating all check functions, call `export_checks!(fn_name, ...)` once at
/// the crate root to wire up the component exports.
#[proc_macro_attribute]
pub fn check(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as CheckArgs);
    let func = parse_macro_input!(item as ItemFn);
    match expand_check(args, func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_check(args: CheckArgs, func: ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = &func.sig.ident;
    let entry_struct = format_ident!("__CheckleftEntry_{}", fn_ident);
    let entry_static = format_ident!("__CHECKLEFT_ENTRY_{}", fn_ident);

    let check_name = &args.name;
    let description = args.description.as_deref().unwrap_or(check_name);

    let severity_tokens = match args.severity {
        SeverityArg::Error => quote! { ::checkleft_check_sdk::Severity::Error },
        SeverityArg::Warning => quote! { ::checkleft_check_sdk::Severity::Warning },
        SeverityArg::Info => quote! { ::checkleft_check_sdk::Severity::Info },
    };

    let access_scope_tokens = match &args.access_scope {
        AccessScopeArg::ModifiedOnly => quote! { ::core::option::Option::None },
        AccessScopeArg::WholeRepo => quote! {
            ::core::option::Option::Some(::checkleft_check_sdk::AccessScope::WholeRepo)
        },
        AccessScopeArg::Globs(patterns) => {
            let pats: Vec<_> = patterns.iter().collect();
            quote! {
                ::core::option::Option::Some(
                    ::checkleft_check_sdk::AccessScope::Globs(
                        ::std::vec![ #( #pats.to_owned() ),* ]
                    )
                )
            }
        }
    };

    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #entry_struct;

        impl ::checkleft_check_sdk::__private::CheckEntry for #entry_struct {
            fn name(&self) -> &'static str { #check_name }

            fn descriptor(&self) -> ::checkleft_check_sdk::CheckDescriptor {
                ::checkleft_check_sdk::CheckDescriptor {
                    name: #check_name.to_owned(),
                    description: #description.to_owned(),
                    default_severity: #severity_tokens,
                    access_scope: #access_scope_tokens,
                }
            }

            fn run(
                &self,
                input: ::checkleft_check_sdk::CheckInput,
            ) -> ::std::vec::Vec<::checkleft_check_sdk::Finding> {
                #fn_ident(input)
            }
        }

        // pub(crate) so the generated export module can reference it via super::
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub(crate) static #entry_static: #entry_struct = #entry_struct;

        // Compile-time signature check.
        const _: fn(::checkleft_check_sdk::CheckInput) -> ::std::vec::Vec<::checkleft_check_sdk::Finding> = #fn_ident;
    })
}

// ---------------------------------------------------------------------------
// export_checks! function-like macro
// ---------------------------------------------------------------------------

struct ExportChecksInput {
    fns: Punctuated<Ident, Token![,]>,
}

impl Parse for ExportChecksInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(ExportChecksInput {
            fns: Punctuated::parse_terminated(input)?,
        })
    }
}

/// Wire up the wasm component exports for all `#[check]`-annotated functions.
///
/// Call this macro exactly once at the crate root listing every function
/// decorated with `#[check]`:
///
/// ```rust,ignore
/// export_checks!(my_check, another_check);
/// ```
///
/// The macro generates `list-checks` and `run-check` wasm component exports
/// so the guest crate compiles as a valid checkleft check component.
///
/// # Requirements
///
/// The check crate's `Cargo.toml` must include `wit-bindgen = "0.51"` as a
/// dependency. The `export_checks!` expansion calls `wit_bindgen::generate!`
/// with the embedded WIT contract.
#[proc_macro]
pub fn export_checks(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as ExportChecksInput);
    match expand_export_checks(parsed) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_export_checks(input: ExportChecksInput) -> syn::Result<TokenStream2> {
    let fns: Vec<&Ident> = input.fns.iter().collect();
    let entry_statics: Vec<proc_macro2::Ident> =
        fns.iter().map(|id| format_ident!("__CHECKLEFT_ENTRY_{}", id)).collect();

    let wit = WIT_CONTENT;

    // list_checks: collect descriptors from all entries
    let list_checks_body: Vec<TokenStream2> = entry_statics
        .iter()
        .map(|stat| {
            quote! {
                to_wit_descriptor(super::#stat.descriptor()),
            }
        })
        .collect();

    // run_check: dispatch arms
    let dispatch_arms: Vec<TokenStream2> = entry_statics
        .iter()
        .map(|stat| {
            quote! {
                n if n == super::#stat.name() => {
                    let sdk_findings = super::#stat.run(sdk_input);
                    ::core::result::Result::Ok(
                        sdk_findings.into_iter().map(to_wit_finding).collect()
                    )
                }
            }
        })
        .collect();

    Ok(quote! {
        // The module is private; the wasm exports it generates are still
        // globally visible because they use #[no_mangle] / component ABI.
        #[doc(hidden)]
        mod __checkleft_exports {
            // Generate WIT guest bindings from the embedded contract.
            // Using `inline:` with the full WIT package declaration causes
            // wit-bindgen to nest types under `checkleft::check::types`
            // (matching the `checkleft:check@0.1.0` package namespace).
            ::wit_bindgen::generate!({
                world: "check",
                inline: #wit,
            });

            // Alias the generated types submodule for brevity.
            // Path is relative to this module: checkleft::check::types
            use checkleft::check::types as W;

            // Bring the CheckEntry trait into scope so we can call .name(),
            // .descriptor(), and .run() on the entry statics from the parent.
            use ::checkleft_check_sdk::__private::CheckEntry as _;

            struct __CheckleftComponent;

            impl Guest for __CheckleftComponent {
                fn list_checks() -> ::std::vec::Vec<W::CheckDescriptor> {
                    ::std::vec![
                        #( #list_checks_body )*
                    ]
                }

                fn run_check(
                    name: ::std::string::String,
                    input: W::CheckInput,
                ) -> ::core::result::Result<
                    ::std::vec::Vec<W::Finding>,
                    W::CheckError,
                > {
                    let sdk_input = from_wit_input(input);
                    match name.as_str() {
                        #( #dispatch_arms )*
                        _ => ::core::result::Result::Err(W::CheckError::UnknownCheck(name)),
                    }
                }
            }

            export!(__CheckleftComponent);

            // ── Type conversions (WIT ↔ SDK types) ──────────────────────

            fn from_wit_input(raw: W::CheckInput) -> ::checkleft_check_sdk::CheckInput {
                ::checkleft_check_sdk::CheckInput::__from_parts(
                    from_wit_changeset(raw.changeset),
                    raw.config_json,
                )
            }

            fn from_wit_changeset(cs: W::ChangeSet) -> ::checkleft_check_sdk::ChangeSet {
                ::checkleft_check_sdk::ChangeSet {
                    changed_files: cs.changed_files.into_iter().map(from_wit_changed_file).collect(),
                    file_diffs: cs.file_diffs.into_iter().map(from_wit_file_diff).collect(),
                    commit_description: cs.commit_description,
                    pr_description: cs.pr_description,
                    change_id: cs.change_id,
                    repository: cs.repository,
                }
            }

            fn from_wit_changed_file(cf: W::ChangedFile) -> ::checkleft_check_sdk::ChangedFile {
                ::checkleft_check_sdk::ChangedFile {
                    path: cf.path,
                    kind: match cf.kind {
                        W::ChangeKind::Added => ::checkleft_check_sdk::ChangeKind::Added,
                        W::ChangeKind::Modified => ::checkleft_check_sdk::ChangeKind::Modified,
                        W::ChangeKind::Deleted => ::checkleft_check_sdk::ChangeKind::Deleted,
                        W::ChangeKind::Renamed => ::checkleft_check_sdk::ChangeKind::Renamed,
                    },
                    old_path: cf.old_path,
                }
            }

            fn from_wit_file_diff(fd: W::FileDiff) -> ::checkleft_check_sdk::FileDiff {
                ::checkleft_check_sdk::FileDiff {
                    path: fd.path,
                    hunks: fd.hunks.into_iter().map(|h| ::checkleft_check_sdk::DiffHunk {
                        old_start: h.old_start,
                        old_lines: h.old_lines,
                        new_start: h.new_start,
                        new_lines: h.new_lines,
                        added_lines: h.added_lines,
                        removed_lines: h.removed_lines,
                    }).collect(),
                }
            }

            fn to_wit_finding(f: ::checkleft_check_sdk::Finding) -> W::Finding {
                W::Finding {
                    severity: to_wit_severity(f.severity),
                    message: f.message,
                    location: f.location.map(|l| W::Location {
                        path: l.path,
                        line: l.line,
                        column: l.column,
                    }),
                    remediations: f.remediations,
                    suggested_fix: f.suggested_fix.map(|sf| W::SuggestedFix {
                        description: sf.description,
                        edits: sf.edits.into_iter().map(|e| W::FileEdit {
                            path: e.path,
                            old_text: e.old_text,
                            new_text: e.new_text,
                        }).collect(),
                    }),
                }
            }

            fn to_wit_severity(s: ::checkleft_check_sdk::Severity) -> W::Severity {
                match s {
                    ::checkleft_check_sdk::Severity::Error => W::Severity::Error,
                    ::checkleft_check_sdk::Severity::Warning => W::Severity::Warning,
                    ::checkleft_check_sdk::Severity::Info => W::Severity::Info,
                }
            }

            fn to_wit_descriptor(d: ::checkleft_check_sdk::CheckDescriptor) -> W::CheckDescriptor {
                W::CheckDescriptor {
                    name: d.name,
                    description: d.description,
                    default_severity: to_wit_severity(d.default_severity),
                    access_scope: d.access_scope.map(|s| match s {
                        ::checkleft_check_sdk::AccessScope::ModifiedOnly => W::AccessScope::ModifiedOnly,
                        ::checkleft_check_sdk::AccessScope::WholeRepo => W::AccessScope::WholeRepo,
                        ::checkleft_check_sdk::AccessScope::Globs(patterns) => W::AccessScope::Globs(patterns),
                    }),
                }
            }
        }
    })
}
