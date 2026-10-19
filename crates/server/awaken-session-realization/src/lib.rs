#![warn(unreachable_pub)]
//! Canonical Session resource realization effects (ADR-0089).
//! Durable truth stays in Session; only process-local handles and projections live here.

pub mod artifact_harvest;
pub mod cache_volume;
pub mod environment_capacity;
pub mod environment_holding;
pub mod environment_owner;
pub mod environment_providers;
pub mod execution_authority;
pub mod frozen_inputs;
pub mod managed_input_layout;
pub mod memory_binding;
pub mod memory_binding_compiler;
pub mod peers;
pub mod realization_error;
pub mod realization_executor;
pub mod realization_state;
pub mod resource_staging;
pub mod sandbox_projection;
pub mod session_environment;
pub mod session_publication;
pub mod session_realizations;
pub mod skill_delivery;
pub mod staged_resources;
pub mod terminal_effects;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub use realization_executor::RealizationExecutor;

pub mod worker_projection;
