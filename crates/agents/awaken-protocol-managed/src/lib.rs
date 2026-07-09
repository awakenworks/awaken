//! `awaken-protocol-managed` — the Managed Agents runtime-protocol adapter.
//!
//! This is the anti-corruption boundary between the public Anthropic Managed
//! Agents wire and the neutral runtime. It owns the public DTOs, the projection
//! from committed `Message`s to public events, and the axum router; it drives one
//! [`SessionRuntime`] port and constructs no runtime itself. It is the only crate
//! permitted to name Anthropic protocol vocabulary (G16).
//!
//! Scope: `POST /v1/sessions` (advertising the runtime's provisioned tool/resource
//! surface), `POST /v1/sessions/{id}/events` (`user.message`, HITL
//! `user.tool_confirmation`, `user.custom_tool_result`, `user.define_outcome`,
//! `user.interrupt`), `GET /v1/sessions/{id}/events`, and the SSE stream.

// The crate is organized by concern:
//
// 1. `types`   — native Managed Agents wire shapes, 1:1 with the TS SDK.
// 2. `project` — conversion/projection: neutral domain state → wire events.
// 3. `router`  — routing: the axum routers and handlers.
// 4. `ext`     — our extensions: vocabulary and surfaces not on the SDK wire.
//
// `state` holds the `SessionRuntime` port and the session record store the router
// drives and the projection writes into. The management-plane resource surfaces
// (`agents_registry`, `deployments`, `environments`, `user_profiles`, `vaults`)
// are self-contained modules — each bundles its own wire types, state, and router
// for one resource — plus the shared `session_repo`/`pagination` helpers.

/// 1. The wire transfer objects: the Managed Agents request/response/event shapes,
/// each mapping 1:1 onto the `@anthropic-ai/sdk` beta `managed-agents` types.
/// Pure serde types only — the logic that *assembles* them from neutral domain
/// state lives in [`project`] and [`state`].
pub mod types;
/// 2. The projection: committed `Message`s and engine events → public wire events.
pub mod project;
/// 3. The routing: the axum router and handlers over [`state::ManagedState`].
mod router;
/// 4. Our extensions: non-SDK vocabulary and surfaces (awaken model/runtime
/// selection, the live-inbox edit protocol) kept apart from the compatible core.
pub mod ext;

mod state;
mod session_repo;
pub mod pagination;

// Management-plane resource surfaces (self-contained per-resource modules).
pub mod agents_registry;
pub mod deployments;
pub mod environments;
pub mod user_profiles;
pub mod vaults;

pub use agents_registry::{AgentConfigSource, AgentConfigView, AgentRegistryState, agents_router};
pub use deployments::{DeploymentState, deployments_router};
pub use environments::{EnvironmentState, environments_router};
pub use router::{ProjectScope, router};
pub use session_repo::{InMemorySessionRepository, ManagedSessionRepository, PersistedSession};
pub use state::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, ManagedState, McpServerBinding, OutcomeIteration, OutcomeReport, Pending,
    RunError, RunErrorKind, SessionInit, SessionResource, SessionRuntime, SessionUsage, StateError,
    TurnFailure, TurnOutcome,
};
pub use user_profiles::{UserProfileState, user_profiles_router};
pub use vaults::{
    McpProbe, McpProbeStatus, McpRefreshBinding, TokenEndpointAuthBinding, VaultState, vault_router,
};
