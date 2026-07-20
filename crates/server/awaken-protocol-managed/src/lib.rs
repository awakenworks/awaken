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
// - `types`   — native Managed Agents wire shapes, 1:1 with the TS SDK.
// - `project` — conversion/projection: neutral domain state → wire events.
// - `routes`  — routing: the axum routers and handlers for every surface
//               (sessions + the management-plane resources), each resource
//               bundling the in-memory store it drives.
// - `ext`     — our extensions: vocabulary and surfaces not on the SDK wire.
//
// `state` holds the `SessionRuntime` port and the session record store the routes
// drive and the projection writes into; `session_repo` is its persistence port.

/// Our extensions: non-SDK vocabulary and surfaces (awaken model/runtime selection,
/// the live-inbox edit protocol) kept apart from the compatible core.
pub mod ext;
/// Conversion/projection: committed `Message`s and engine events → public wire events.
pub mod project;
/// Routing: the axum routers and handlers for every surface, over [`state::ManagedState`]
/// and the resource stores.
mod routes;
/// Native Managed Agents wire transfer objects, 1:1 with the `@anthropic-ai/sdk`
/// beta `managed-agents` types. Pure serde shapes; the logic that *assembles* them
/// from neutral domain state lives in [`project`] and [`state`].
pub mod types;

/// The self-hosted environment registry as a port ([`env_registry::EnvRegistry`]),
/// with an in-memory default; durable (sqlite/postgres) backends fold in behind it.
pub mod cron;
pub mod env_registry;
mod preview;
mod state;
/// The self-hosted environment work queue as a port ([`work_queue::WorkQueue`]),
/// with an in-memory default; durable (sqlite/postgres) backends fold in behind it.
pub mod work_queue;

pub use env_registry::{EnvItem, EnvRegistry, EnvUpdate, InMemoryEnvRegistry};
pub use routes::agents_registry::{
    AgentConfigSource, AgentConfigView, AgentMcpServerView, AgentRegistryState, agents_router,
};
pub use routes::deployments::{DeploymentState, deployments_router};
pub use routes::environments::{EnvironmentState, environments_router};
pub use routes::user_profiles::{UserProfileState, user_profiles_router};
pub use routes::vaults::{
    McpProbe, McpProbeStatus, McpRefreshBinding, TokenEndpointAuthBinding, VaultState, vault_router,
};
pub use routes::{WorkspaceScope, enforce_managed_beta, router};
// The session-repository port family now lives in `awaken-session-contract`;
// re-exported so existing `awaken_protocol_managed::…` paths keep resolving.
pub use awaken_session_contract::{
    ManagedSessionRepository, PersistedSession, ScopedSessionRepo, ScopedSessionStore,
};
pub use awaken_session_store::{InMemoryScopedSessionStore, InMemorySessionRepository};
pub use state::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, ManagedState, McpServerBinding, OutcomeIteration, OutcomeReport, Pending,
    RunError, RunErrorKind, SessionInit, SessionLifecycleSink, SessionResource, SessionRuntime,
    SessionUsage, StateError, StepOutcome, ToolPermissionDecision,
};
pub use work_queue::{WorkItem, WorkQueue, WorkState};
