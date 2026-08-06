//! `awaken-protocol-managed` — the complete Managed Public API protocol adapter.
//!
//! This is the anti-corruption boundary between the public Anthropic Managed
//! Agents wire and the neutral runtime. It owns the public DTOs, the projection
//! from committed `Message`s to public events, and the axum router; it drives one
//! [`SessionRuntime`](awaken_session_contract::SessionRuntime) port and constructs no runtime itself. It is the only crate
//! permitted to name Anthropic protocol vocabulary (G16).
//!
//! Scope: `POST /v1/sessions` (advertising the runtime's provisioned tool/resource
//! surface), `POST /v1/sessions/{id}/events` (`user.message`, HITL
//! `user.tool_confirmation`, `user.custom_tool_result`, `user.define_outcome`,
//! `user.interrupt`), `GET /v1/sessions/{id}/events`, and the SSE stream.

// The crate is organized by concern:
//
// - `types`   — native Managed Agents wire shapes, 1:1 with the TS SDK.
// - `project` — conversion/projection: neutral domain state → wire events.
// - `routes`  — routing: the axum routers and handlers for every surface
//               (sessions + the management-plane resources), each resource
//               bundling the in-memory store it drives.
// `state` holds the `SessionRuntime` port and the session record store the routes
// drive and the projection writes into; `session_repo` is its persistence port.
mod common;
mod control;
/// Conversion/projection: committed `Message`s and engine events → public wire events.
pub mod project;
mod rate_limit;
/// Routing: the axum routers and handlers for every surface, over [`state::ManagedState`]
/// and the resource stores.
mod routes;
/// Native Managed Agents wire transfer objects, 1:1 with the `@anthropic-ai/sdk`
/// beta `managed-agents` types. Pure serde shapes; the logic that *assembles* them
/// from neutral domain state lives in [`project`] and the private adapter state.
pub mod types;

pub use common::headers::MANAGED_BETA;
pub use control::{
    ModelDirectory, ModelDirectoryFuture, ModelEntry, default_models, models_router,
    models_router_with_directory,
};
mod resources;
pub use resources::{
    ResourcesRouterInput, files_router, memory_stores_router, resources_router, skills_router,
};

/// Managed Environment wire projection over the Control-owned
/// [`env_registry::EnvRegistry`] contract and its durable adapters.
mod dream;
mod env_registry;
mod preview;
mod state;
/// The self-hosted environment work queue as a port ([`work_queue::WorkQueue`]),
/// with an in-memory default; durable (sqlite/postgres) backends fold in behind it.
mod work_queue;

pub use rate_limit::{ManagedRateLimiter, ManagedRateLimits, enforce_managed_rate_limit};
pub use routes::agents_registry::{
    AgentRegistryState, ManagedAgentError, ManagedAgentRepository, agents_router,
};
pub use routes::deployments::{LocalDeploymentSessionLauncher, deployments_router};
pub use routes::environments::{
    CoordinatorEnvironmentRegistrar, EnvironmentAuthoringState, EnvironmentExecutionState,
    EnvironmentState, environment_authoring_router, environment_work_router, environments_router,
};
pub use routes::user_profiles::{UserProfileState, user_profiles_router};
pub use routes::vaults::{VaultState, vault_router};
pub use routes::{DREAMING_BETA, dreams_router};
pub use routes::{MEMORY_BETA, SKILLS_BETA, enforce_managed_beta, router};
pub use state::{ManagedState, StateError};
