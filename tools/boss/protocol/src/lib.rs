//! Wire-level types shared between `boss-engine` and the `boss` CLI.
//!
//! Anything that goes over the engine's frontend socket — both the
//! request/response envelope and the data shapes those carry — lives in this
//! crate so that engine and clients link against the same types.

mod engine_app;
mod live_status_debug;
mod live_worker_state;
mod types;
mod wire;
mod worker_event;
mod worker_names;

pub use engine_app::*;
pub use live_status_debug::*;
pub use live_worker_state::*;
pub use types::*;
pub use wire::*;
pub use worker_event::*;
pub use worker_names::*;
