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
// - `routes`  — routing: axum handlers decode DTOs, call injected applications,
//               and encode wire responses; no resource business state lives here.
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

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use common::headers::{
    LEGACY_TUNNELS_BETA, MANAGED_BETA, TUNNELS_BETA, USER_PROFILES_BETA,
    parse_idempotency_key_header,
};
pub use control::{ModelEntry, default_models, models_router, models_router_with_inventory};
mod resources;
pub use resources::{
    ResourcesRouterInput, files_router, memory_stores_router, resources_router, skills_router,
};

/// Managed Environment wire projection over the Control-owned
/// [`env_registry::EnvRegistry`] contract and its durable adapters.
mod dream;
mod env_registry;
mod inference_policy;
mod preview;
mod state;
mod tunnel;
/// The self-hosted environment work queue as a port ([`work_queue::WorkQueue`]),
/// with an in-memory default; durable (sqlite/postgres) backends fold in behind it.
mod work_queue;

pub use inference_policy::{
    InferenceGeoCheckpoint, InferenceGeoPolicyError, ManagedInferenceGeoPolicy, inference_geo_name,
};
pub use rate_limit::{
    ManagedOperation, ManagedRateLimitDecision, ManagedRateLimitRequest,
    ManagedRateLimitUnavailable, ManagedRateLimiter, ManagedRateLimits, ManagedRequestLimiter,
    ManagedRequestSource, enforce_managed_rate_limit,
};
pub use routes::agents_registry::{
    AgentRegistryState, ManagedAgentError, ManagedAgentRepository, agents_router,
};
pub use routes::credential_rollouts::{
    HttpManagedCredentialRolloutTarget, credential_rollout_router_with_authenticator,
};
pub use routes::deployments::{ManagedDeploymentSessionLauncher, deployments_router};
pub use routes::environments::{
    EnvironmentAuthoringState, environment_authoring_router, environment_work_router,
};
pub use routes::user_profiles::user_profiles_router;
pub use routes::vaults::{VaultState, vault_router};
pub use routes::{
    ANTHROPIC_API_VERSION, MEMORY_BETA, SKILLS_BETA, create_profiled_session, enforce_managed_beta,
    replace_resource_manifest, router, tunnels_router,
};
pub use routes::{DREAMING_BETA, dreams_router};
pub use state::{ManagedState, StateError};
pub use tunnel::{ManagedTunnelApplication, ManagedTunnelApplicationError, ManagedTunnelScope};
