//! Proc-macro support for `checkleft-check-sdk`.
//!
//! Provides the `#[check]` attribute macro for annotating check functions and
//! the `export_checks!` function-like macro for wiring the guest component.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprArray, ExprLit, Ident, ItemFn, Lit, LitStr, Path, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
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
    /// Optional function to call for `declared_exclusions` dispatch.
    declared_exclusions: Option<Ident>,
    /// Optional function to call for `evaluate_exclusion` dispatch.
    evaluate_exclusion: Option<Ident>,
    /// Optional function to call for `declare_required_files` dispatch.
    required_files: Option<Ident>,
    /// Optional function to call for `fix-check` dispatch.
    fix: Option<Ident>,
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
    DeclaredFiles,
}

impl Parse for CheckArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name: Option<String> = None;
        let mut description: Option<String> = None;
        let mut severity = SeverityArg::Warning;
        let mut access_scope = AccessScopeArg::ModifiedOnly;
        let mut declared_exclusions: Option<Ident> = None;
        let mut evaluate_exclusion: Option<Ident> = None;
        let mut required_files: Option<Ident> = None;
        let mut fix: Option<Ident> = None;

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
                "declared_exclusions" => {
                    let ident: Ident = input.parse()?;
                    declared_exclusions = Some(ident);
                }
                "evaluate_exclusion" => {
                    let ident: Ident = input.parse()?;
                    evaluate_exclusion = Some(ident);
                }
                "required_files" => {
                    let ident: Ident = input.parse()?;
                    required_files = Some(ident);
                }
                "fix" => {
                    let ident: Ident = input.parse()?;
                    fix = Some(ident);
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
            declared_exclusions,
            evaluate_exclusion,
            required_files,
            fix,
        })
    }
}

fn parse_access_scope(input: ParseStream<'_>) -> syn::Result<AccessScopeArg> {
    let ident: Ident = input.parse()?;
    match ident.to_string().as_str() {
        "modified_only" => Ok(AccessScopeArg::ModifiedOnly),
        "whole_repo" => Ok(AccessScopeArg::WholeRepo),
        "declared_files" => Ok(AccessScopeArg::DeclaredFiles),
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
            format!(
                "unknown access_scope `{other}`: expected `modified_only`, `whole_repo`, `declared_files`, or `globs([...])`"
            ),
        )),
    }
}

/// Annotate a function as a checkleft check.
///
/// # Arguments
/// - `name = "..."` (required): The check name used for dispatch.
/// - `description = "..."` (optional): Human-readable description; defaults to name.
/// - `severity = warning|error|info` (optional): Default severity (default: `warning`).
/// - `access_scope = modified_only|whole_repo|declared_files|globs([...])` (optional):
///   File-access scope (default: `modified_only`). Use `declared_files` together with
///   `required_files = fn` to declare per-changeset file sets without requesting whole-repo
///   access.
/// - `declared_exclusions = fn` (optional): Function to call for stale-exclusion auditing.
///   The function must have the signature `fn(&str) -> Vec<DeclaredExclusion>`.
/// - `evaluate_exclusion = fn` (optional): Function to evaluate whether a declared
///   exclusion is still load-bearing.
///   The function must have the signature `fn(&str, &DeclaredExclusion, Option<&str>) -> ExclusionStatus`.
/// - `required_files = fn` (optional): Function to declare the extra files needed by the
///   check beyond the changeset. Use with `access_scope = declared_files`.
///   The function must have the signature `fn(&ChangeSet, &str) -> Vec<String>`.
/// - `fix = fn` (optional): Function that computes this check's automatic fix for
///   `checkleft fix`. The function takes the same `CheckInput` as the check and
///   returns either `Vec<FileEdit>` (cannot fail) or `Result<Vec<FileEdit>, String>`
///   (may fail with a message). The host validates that every returned edit targets
///   a file in the fixable set and applies the edits itself — the guest never writes
///   the filesystem. Absent ⇒ the check has no fix (a no-op for `checkleft fix`).
///
/// After annotating all check functions, call `export_checks!(path::to::fn, ...)` once at
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
        AccessScopeArg::DeclaredFiles => quote! {
            ::core::option::Option::Some(::checkleft_check_sdk::AccessScope::DeclaredFiles)
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

    // Optional declared_exclusions trait method impl.
    let declared_exclusions_impl = if let Some(hook_fn) = &args.declared_exclusions {
        quote! {
            fn declared_exclusions(
                &self,
                config_json: &str,
            ) -> ::std::vec::Vec<::checkleft_check_sdk::DeclaredExclusion> {
                #hook_fn(config_json)
            }
        }
    } else {
        quote! {}
    };

    // Optional evaluate_exclusion trait method impl.
    let evaluate_exclusion_impl = if let Some(hook_fn) = &args.evaluate_exclusion {
        quote! {
            fn evaluate_exclusion(
                &self,
                config_json: &str,
                excl: &::checkleft_check_sdk::DeclaredExclusion,
                file_content: ::core::option::Option<&str>,
            ) -> ::checkleft_check_sdk::ExclusionStatus {
                #hook_fn(config_json, excl, file_content)
            }
        }
    } else {
        quote! {}
    };

    // Optional declare_required_files trait method impl.
    let required_files_impl = if let Some(hook_fn) = &args.required_files {
        quote! {
            fn declare_required_files(
                &self,
                changeset: &::checkleft_check_sdk::ChangeSet,
                config_json: &str,
            ) -> ::std::vec::Vec<::std::string::String> {
                #hook_fn(changeset, config_json)
            }
        }
    } else {
        quote! {}
    };

    // Optional fix trait method impl. The fixer may return `Vec<FileEdit>` or
    // `Result<Vec<FileEdit>, String>`; `IntoFixOutcome` normalizes both shapes,
    // so this expansion does not depend on which one the author chose.
    let fix_impl = if let Some(hook_fn) = &args.fix {
        quote! {
            fn fix(
                &self,
                input: ::checkleft_check_sdk::CheckInput,
            ) -> ::checkleft_check_sdk::FixOutcome {
                ::checkleft_check_sdk::IntoFixOutcome::into_fix_outcome(#hook_fn(input))
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #func

        // `pub` (not private) so an aggregating bundle crate can reference the
        // entry via `export_checks!` across a crate boundary. The companion
        // `static` below is also `pub`, and a `pub static` of a private type is
        // an error (E0446); `#[doc(hidden)]` keeps both out of the public docs
        // and the `missing_docs` lint.
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #entry_struct;

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

            #declared_exclusions_impl
            #evaluate_exclusion_impl
            #required_files_impl
            #fix_impl
        }

        // `pub` so both the same-crate `export_checks!` (via `super::`) and an
        // out-of-crate aggregating bundle (via `crate_name::__CHECKLEFT_ENTRY_fn`)
        // can reference it. See the `pub struct` note above.
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub static #entry_static: #entry_struct = #entry_struct;

        // Compile-time signature check.
        const _: fn(::checkleft_check_sdk::CheckInput) -> ::std::vec::Vec<::checkleft_check_sdk::Finding> = #fn_ident;
    })
}

// ---------------------------------------------------------------------------
// export_checks! function-like macro
// ---------------------------------------------------------------------------

/// One item in the `export_checks!(...)` argument list: a path to a
/// `#[check]`-annotated function.
struct ExportChecksItem {
    path: Path,
}

impl Parse for ExportChecksItem {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let path: Path = input.parse()?;
        Ok(ExportChecksItem { path })
    }
}

struct ExportChecksInput {
    items: Vec<ExportChecksItem>,
}

impl Parse for ExportChecksInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut items = Vec::new();
        while !input.is_empty() {
            items.push(input.parse::<ExportChecksItem>()?);
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(ExportChecksInput { items })
    }
}

/// Wire up the wasm component exports for all `#[check]`-annotated functions.
///
/// Call this macro exactly once at the crate root, listing every check function
/// you want to export. Each argument is a path to a `#[check]`-annotated function:
///
/// **Same-crate (single-check component):**
/// ```rust,ignore
/// export_checks!(my_check);
/// ```
///
/// **Cross-crate bundle (multiple checks from different crates):**
/// ```rust,ignore
/// export_checks!(
///     some_check_crate::my_check,
///     another_check_crate::other_check,
/// );
/// ```
///
/// The macro generates `list-checks` and `run-check` wasm component exports
/// so the crate compiles as a valid checkleft check component.
///
/// Stale-exclusion audit hooks and required-files hooks are picked up
/// automatically from the `#[check]` annotation on each function (via
/// `declared_exclusions = fn`, `evaluate_exclusion = fn`, and
/// `required_files = fn` arguments). No separate `exclusion_audit(...)` or
/// `required_files(...)` entries are needed in `export_checks!`.
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

/// Build the token stream that references the `__CHECKLEFT_ENTRY_<fn>` static
/// for a given check function path.
///
/// - For a single-ident path (`my_check`): produces `super::__CHECKLEFT_ENTRY_my_check`
///   so the reference works inside the generated inner module.
/// - For a multi-segment path (`some_crate::my_check`): produces
///   `some_crate::__CHECKLEFT_ENTRY_my_check`, referencing the exported static
///   directly from the check crate.
fn entry_static_ref(path: &Path) -> syn::Path {
    let fn_ident = &path.segments.last().unwrap().ident;
    let entry_ident = format_ident!("__CHECKLEFT_ENTRY_{}", fn_ident);

    if path.segments.len() == 1 {
        // Same-crate: reference via super (we're inside __checkleft_exports module)
        syn::parse_quote!(super::#entry_ident)
    } else {
        // Cross-crate: replace the last path segment with the entry static ident
        let mut entry_path = path.clone();
        entry_path.segments.last_mut().unwrap().ident = entry_ident;
        entry_path
    }
}

fn expand_export_checks(input: ExportChecksInput) -> syn::Result<TokenStream2> {
    let entry_paths: Vec<syn::Path> = input.items.iter().map(|item| entry_static_ref(&item.path)).collect();

    let wit = WIT_CONTENT;

    // list_checks: collect descriptors from all entries
    let list_checks_body: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                to_wit_descriptor(#entry.descriptor()),
            }
        })
        .collect();

    // run_check: match-arm dispatch
    let dispatch_arms: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                n if n == #entry.name() => {
                    let sdk_findings = #entry.run(sdk_input);
                    ::core::result::Result::Ok(
                        sdk_findings.into_iter().map(to_wit_finding).collect()
                    )
                }
            }
        })
        .collect();

    // fix_check: match-arm dispatch, mirroring run_check. Each arm maps the
    // check's `FixOutcome` onto the `fix-check` WIT result.
    let fix_dispatch_arms: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                n if n == #entry.name() => {
                    match #entry.fix(sdk_input) {
                        ::checkleft_check_sdk::FixOutcome::Edits(edits) => ::core::result::Result::Ok(
                            edits.into_iter().map(to_wit_file_edit).collect()
                        ),
                        ::checkleft_check_sdk::FixOutcome::Failed(message) =>
                            ::core::result::Result::Err(W::FixError::Failed(message)),
                        ::checkleft_check_sdk::FixOutcome::NotFixable =>
                            ::core::result::Result::Err(W::FixError::NotFixable),
                    }
                }
            }
        })
        .collect();

    // declared_exclusions: if-chain dispatch (hooks travel with the check entry)
    let decl_excl_arms: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                if name.as_str() == #entry.name() {
                    return #entry.declared_exclusions(&config_json)
                        .into_iter()
                        .map(to_wit_declared_exclusion)
                        .collect();
                }
            }
        })
        .collect();

    // evaluate_exclusion: if-chain dispatch
    let eval_excl_arms: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                if name.as_str() == #entry.name() {
                    return to_wit_exclusion_status(#entry.evaluate_exclusion(
                        &config_json,
                        &sdk_excl,
                        file_content.as_deref(),
                    ));
                }
            }
        })
        .collect();

    // declare_required_files: if-chain dispatch
    // Each arm consumes `changeset` only when the name matches; Rust's
    // flow-sensitive borrow checker accepts this because each arm returns.
    let req_files_arms: Vec<TokenStream2> = entry_paths
        .iter()
        .map(|entry| {
            quote! {
                if name.as_str() == #entry.name() {
                    let sdk_changeset = from_wit_changeset(changeset);
                    return #entry.declare_required_files(&sdk_changeset, &config_json);
                }
            }
        })
        .collect();

    Ok(quote! {
        // The module is private; the wasm exports it generates are still
        // globally visible because they use #[no_mangle] / component ABI.
        #[doc(hidden)]
        #[allow(clippy::too_many_arguments)] // wit_bindgen::generate! emits functions with many args from the WIT contract
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
            // .descriptor(), .run(), .declared_exclusions(), etc. on entry statics.
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

                fn fix_check(
                    name: ::std::string::String,
                    input: W::CheckInput,
                ) -> ::core::result::Result<
                    ::std::vec::Vec<W::FileEdit>,
                    W::FixError,
                > {
                    let sdk_input = from_wit_input(input);
                    match name.as_str() {
                        #( #fix_dispatch_arms )*
                        _ => ::core::result::Result::Err(W::FixError::UnknownCheck(name)),
                    }
                }

                fn declare_required_files(
                    name: ::std::string::String,
                    changeset: W::ChangeSet,
                    config_json: ::std::string::String,
                ) -> ::std::vec::Vec<::std::string::String> {
                    #( #req_files_arms )*
                    ::std::vec::Vec::new()
                }

                fn declared_exclusions(
                    name: ::std::string::String,
                    config_json: ::std::string::String,
                ) -> ::std::vec::Vec<W::DeclaredExclusion> {
                    #( #decl_excl_arms )*
                    ::std::vec::Vec::new()
                }

                fn evaluate_exclusion(
                    name: ::std::string::String,
                    config_json: ::std::string::String,
                    exclusion: W::DeclaredExclusion,
                    file_content: ::core::option::Option<::std::string::String>,
                ) -> W::ExclusionStatus {
                    let sdk_excl = from_wit_declared_exclusion(exclusion);
                    #( #eval_excl_arms )*
                    W::ExclusionStatus::Unknown
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
                    base_files: cs.base_files.into_iter().map(|bf| ::checkleft_check_sdk::BaseFile {
                        path: bf.path,
                        content: bf.content,
                    }).collect(),
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
                        edits: sf.edits.into_iter().map(to_wit_file_edit).collect(),
                    }),
                }
            }

            fn to_wit_file_edit(e: ::checkleft_check_sdk::FileEdit) -> W::FileEdit {
                W::FileEdit {
                    path: e.path,
                    old_text: e.old_text,
                    new_text: e.new_text,
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
                        ::checkleft_check_sdk::AccessScope::DeclaredFiles => W::AccessScope::DeclaredFiles,
                        ::checkleft_check_sdk::AccessScope::Globs(patterns) => W::AccessScope::Globs(patterns),
                    }),
                }
            }

            fn to_wit_declared_exclusion(e: ::checkleft_check_sdk::DeclaredExclusion) -> W::DeclaredExclusion {
                W::DeclaredExclusion {
                    entry: e.entry,
                    depends_on: e.depends_on,
                }
            }

            fn from_wit_declared_exclusion(e: W::DeclaredExclusion) -> ::checkleft_check_sdk::DeclaredExclusion {
                ::checkleft_check_sdk::DeclaredExclusion {
                    entry: e.entry,
                    depends_on: e.depends_on,
                }
            }

            fn to_wit_exclusion_status(s: ::checkleft_check_sdk::ExclusionStatus) -> W::ExclusionStatus {
                match s {
                    ::checkleft_check_sdk::ExclusionStatus::LoadBearing => W::ExclusionStatus::LoadBearing,
                    ::checkleft_check_sdk::ExclusionStatus::Stale(reason) => W::ExclusionStatus::Stale(reason),
                    ::checkleft_check_sdk::ExclusionStatus::Unknown => W::ExclusionStatus::Unknown,
                }
            }
        }
    })
}
