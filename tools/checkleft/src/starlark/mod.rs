//! Starlark-backed check infrastructure.
//!
//! This module starts with the narrow, buildable core needed to evaluate one
//! `.checkleft` file against a text evolution context. Discovery, package
//! manifests, richer adapters, and fix evaluation build on this foundation.

pub mod discovery;
mod evaluator;
mod loader;
pub mod manifest;

pub use evaluator::{StarlarkCheckRunner, StarlarkCheckSource};
