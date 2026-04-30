//! Wire-level types shared between `boss-engine` and the `boss` CLI.
//!
//! Anything that goes over the engine's frontend socket — both the
//! request/response envelope and the data shapes those carry — lives in this
//! crate so that engine and clients link against the same types.

mod types;
mod wire;
mod worker_event;

pub use types::*;
pub use wire::*;
pub use worker_event::*;
