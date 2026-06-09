//! Wire-level types shared between `boss-engine` and the `boss` CLI.
//!
//! Anything that goes over the engine's frontend socket — both the
//! request/response envelope and the data shapes those carry — lives in this
//! crate so that engine and clients link against the same types.

mod engine_app;
mod health_wire;
mod host_registry_wire;
mod live_status_debug;
mod live_worker_state;
mod metrics_wire;
pub mod planner;
mod types;
mod wire;
mod worker_event;
mod worker_names;

pub use engine_app::*;
pub use health_wire::*;
pub use host_registry_wire::*;
pub use live_status_debug::*;
pub use live_worker_state::*;
pub use metrics_wire::*;
pub use planner::{
    ApplyResult, Confidence, DocRef, PlannerInput, PlannerOutput, ProductContext, ProjectContext,
    ProposedEdge, ProposedTask, TaskBrief, planner_output_schema,
};
pub use types::*;
pub use wire::*;
pub use worker_event::*;
pub use worker_names::*;
