//! `checkleft fix` — the write-side companion to `run`.
//!
//! This module hosts the machinery that *applies* fixes to the working tree.
//! The headline property is safety: a fix may only ever write files in its own
//! fixable set, a failed fix leaves the originals untouched, and no write is ever
//! partial. That guarantee is structural, not best-effort — it is enforced by the
//! [`safety`] core, which stages a writable copy sandbox, runs the fixer there,
//! and atomically copies back only the files that actually changed.
//!
//! At this stage the module is *pure mechanism*: no fixers (declarative tool,
//! WASM entry point, or built-in `suggested_fix`) are wired in yet. Those land in
//! later tasks and all funnel through [`safety::WritableSandbox`].

pub mod safety;
pub mod scheduler;
#[cfg(test)]
mod tests;

pub use safety::{CopyBackReport, WritableSandbox};
pub use scheduler::{FixGroup, build_fix_schedule};

/// The outcome of invoking a WASM/component check's `fix-check` entry point and
/// routing its edits through the [`safety`] copy-back core.
///
/// This is the host-side companion to the guest's `fix-error` WIT result: a real
/// fixer failure (or an edit that targets a file outside the fixable set) surfaces
/// as an `Err` from the runtime, while `not-fixable` — the ordinary outcome for a
/// check with no declared fix — is the non-error [`ComponentFixOutcome::NotFixable`].
#[derive(Debug)]
pub enum ComponentFixOutcome {
    /// The check produced edits that were applied through the copy-back core. The
    /// [`CopyBackReport`] names exactly the files written to the real tree (a
    /// subset of the fixable set that actually changed) and any copy-back error.
    Applied(CopyBackReport),
    /// The check declares no fix entry point. A no-op for `fix`, not an error.
    NotFixable,
}
