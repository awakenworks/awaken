//! `awaken-protocol-managed` — the complete Managed Public API protocol adapter.
//!
//! This is the anti-corruption boundary between the public Anthropic Managed
//! Agents wire and the neutral runtime. It owns the public DTOs, the projection
//! from committed `Message`s to public events, and the axum router; it drives one
//! [`SessionRuns`](awaken_session_execution_contract::SessionRuns) port and constructs no runtime itself. It is the only crate
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
// `state` holds the `SessionRuns` port and the session record store the routes
// drive and the projection writes into; `session_repo` is its persistence port.
/// Awaken's `/v1/awaken` extensions of this protocol.
pub mod awaken_extensions;
mod common;
mod control;
/// Conversion/projection: committed `Message`s and engine events → public wire events.
pub mod project;
mod rate_limit;
/// Routing: the axum routers and handlers for every surface, over [`state::ManagedState`]
/// and the resource stores.
mod routes;
#[cfg(feature = "schema")]
pub mod schema;
pub mod tools;
/// Native Managed Agents wire transfer objects, 1:1 with the `@anthropic-ai/sdk`
/// beta `managed-agents` types. Pure serde shapes; the logic that *assembles* them
/// from neutral domain state lives in [`project`] and the private adapter state.
pub mod types;

pub use common::headers::{
    ANTHROPIC_API_VERSION, DREAMING_BETA, LEGACY_TUNNELS_BETA, MANAGED_BETA, MEMORY_BETA,
    ManagedCapability, SKILLS_BETA, TUNNELS_BETA, USER_PROFILES_BETA, parse_idempotency_key_header,
};
pub use common::with_managed_response_context;
pub use common::with_managed_workspace_header;
pub use common::with_unscoped_managed_response_context;
pub use control::{ModelEntry, default_models, models_router, models_router_with_inventory};
mod resources;
pub use resources::{
    ResourcesRouterInput, files_router, memory_stores_router, repositories_router,
    resources_router, skills_router,
};

mod dream;
/// `BetaEnvironment` wire projection of the neutral Environment record.
mod env_registry;
mod inference_policy;
mod lifecycle_delivery;
mod preview;
mod state;
mod tunnel;
/// `BetaSelfHostedWork` wire projection of the neutral WorkQueue item.
mod work_queue;
mod work_session_access;

#[cfg(test)]
mod official_request_witnesses;

pub use inference_policy::{
    InferenceGeoCheckpoint, InferenceGeoPolicyError, ManagedInferenceGeoPolicy, inference_geo_name,
};
pub use lifecycle_delivery::ManagedLifecycleFactDelivery;
pub use rate_limit::{
    annotate_managed_rate_limit, managed_admission_rejected, managed_admission_scope_missing,
    managed_admission_unavailable, managed_org_admission_class,
};
pub use routes::agents_registry::{
    AgentArchiveReceipt, AgentExecution, AgentExecutionEvidence, AgentModelProvisioning,
    AgentRegistryState, ManagedAgentError, ManagedAgentRepository, agents_router,
};
pub use routes::deployments::{ManagedDeploymentSessionLauncher, deployments_router};
pub use routes::dreams_router;
pub use routes::environments::{environment_authoring_router, environment_work_router};
pub use routes::user_profiles::user_profiles_router;
pub use routes::vaults::{McpCredentialChoice, McpInspectionView, VaultState, vault_router};
pub use routes::{
    close_event_admission, create_profiled_session, enforce_managed_beta,
    get_profiled_session_run_events, get_resource_manifest, get_session_content_capture,
    get_session_execution_evidence, list_thread_messages, release_profiled_session,
    replace_resource_manifest, resume_profiled_session_run, router, run_evidence_at,
    session_timeline, submit_profiled_session_run, tunnels_router,
};
pub use state::{
    AdmittedPrivateSessionRequest, COMMITTED_PREFIX_READ_CONFLICT, ManagedRequestState,
    ManagedState, StateError, admitted_private_session_view, managed_session_id_from_idempotency,
};
pub use tunnel::{ManagedTunnelApplication, ManagedTunnelApplicationError, ManagedTunnelScope};
pub use work_session_access::{ManagedWorkSessionAccess, work_session_guard};

pub(crate) use awaken_session_configuration_contract::mcp as session_mcp;

pub(crate) use awaken_session_configuration_contract::resource as session_resource;

mod event_presentation;
pub use event_presentation::HideSessionEventDefinitions;

pub use common::api_version::managed_agent_page_response;
