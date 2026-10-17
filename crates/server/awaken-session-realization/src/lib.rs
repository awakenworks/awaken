#![warn(unreachable_pub)]
//! Canonical Session resource realization effects (ADR-0089).
//! Durable truth stays in Session; only process-local handles and projections live here.

pub mod bound_environment;
pub mod cache_volume;
pub mod environment_capacity;
pub mod environment_continuation;
pub mod environment_holding;
pub mod environment_lifecycle;
pub mod environment_owner;
pub mod environment_providers;
pub mod environment_realization;
pub mod frozen_inputs;
pub mod managed_input_layout;
pub mod managed_resource_projection;
pub mod memory_binding;
pub mod provisioning;
pub mod realization_error;
pub mod realization_executor;
pub mod resource_staging;
pub mod session_environment;
pub mod session_publication;
pub mod session_realizations;
pub mod skill_delivery;
pub mod staged_resources;
pub mod terminal_cleanup;
pub mod terminal_repository_publication;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub use realization_executor::RealizationExecutor;

mod dispatched_resources;

pub mod worker_projection;
