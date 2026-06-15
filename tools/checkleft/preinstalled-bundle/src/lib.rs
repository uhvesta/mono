//! The single multiplexed Component Model component that ships embedded in the
//! checkleft binary, exporting every PREINSTALLED wasm check.
//!
//! # Why a bundle
//!
//! Each preinstalled check used to be its own `cdylib` compiled to its own
//! component and `include_bytes!`'d into the binary. Every component statically
//! linked its OWN copy of the wasm `std`/`core`/`alloc` runtime, the
//! `checkleft-check-sdk`, the `wit-bindgen` glue, and `serde`/`serde_json`, so
//! the embedded size grew by a full shared baseline with every new check (and
//! heavy deps like `syn` were duplicated across checks that use them).
//!
//! This crate is a single `cdylib` that depends on each check's source crate as
//! an rlib and calls [`export_checks!`] exactly once. Rust's LTO / dead-code
//! elimination then dedups the shared baseline and shared deps across all checks
//! inside ONE component. The SDK already supports multiple checks per component:
//! the generated `list-checks` / `run-check` exports dispatch by check name, and
//! the host (`tools/checkleft/src/external/runtime.rs`) already drives them that
//! way — so this is purely a packaging change. Check ids, messages, severities,
//! and behavior are unchanged.
//!
//! # Per-invocation isolation
//!
//! The host re-instantiates a fresh component instance for every `run-check`
//! call (cheap relative to the AOT compile, which is cached once per component).
//! Sharing one module across checks does not weaken that isolation: one check's
//! panic or memory growth cannot affect a sibling, because each call runs in its
//! own instance with its own linear memory and WASI sandbox.
//!
//! # Boundary: preinstalled only
//!
//! This consolidation is for the in-binary preinstalled set ONLY. It deliberately
//! does NOT change the path for externally-distributed checks, which are loaded
//! at runtime as their own separate components (see
//! `tools/checkleft/src/external/runtime.rs` and the standalone-component build
//! demonstrated by `tools/checkleft/sdk/examples/trivial-check`). Each preinstalled
//! check keeps its own authorship crate under `tools/checkleft/checks/<ns>/<name>/`;
//! only the component packaging is shared here.

use checkleft_check_sdk::export_checks;

// Bring each preinstalled check's generated component-ABI entry into this crate's
// root so the single `export_checks!` below can register it. `#[check]` in each
// source crate emits a `__CHECKLEFT_ENTRY_<fn>` static; `export_checks!` derives
// that name from the function ident it is given and references it via `super::`.
// The `rust/giant-structs` stale-exclusion audit hooks are plain functions.
use checkleft_file_forbidden_path::__CHECKLEFT_ENTRY_forbidden_path_check;
use checkleft_file_ifchange::__CHECKLEFT_ENTRY_file_ifchange_check;
use checkleft_file_size::__CHECKLEFT_ENTRY_file_size_check;
use checkleft_rust_giant_structs_create::{
    __CHECKLEFT_ENTRY_giant_structs_create_check, giant_structs_create_declared_exclusions,
    giant_structs_create_evaluate_exclusion,
};
use checkleft_rust_giant_structs_use_builder::{
    __CHECKLEFT_ENTRY_giant_structs_check, giant_structs_declared_exclusions, giant_structs_evaluate_exclusion,
};

export_checks!(
    forbidden_path_check,
    file_size_check,
    file_ifchange_check,
    giant_structs_check,
    exclusion_audit(
        "rust/giant-structs",
        giant_structs_declared_exclusions,
        giant_structs_evaluate_exclusion
    ),
    giant_structs_create_check,
    exclusion_audit(
        "rust/giant-structs-create",
        giant_structs_create_declared_exclusions,
        giant_structs_create_evaluate_exclusion
    ),
);
