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

pub use safety::{CopyBackReport, WritableSandbox};
