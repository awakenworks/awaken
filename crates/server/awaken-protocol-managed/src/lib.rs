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

pub mod dto;
pub mod project;
mod router;
mod session_repo;
mod state;
pub mod vaults;

pub use router::{ProjectScope, router};
pub use session_repo::{InMemorySessionRepository, ManagedSessionRepository, PersistedSession};
pub use state::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, ManagedState, McpServerBinding, OutcomeIteration, OutcomeReport, Pending,
    RunError, RunErrorKind, SessionInit, SessionResource, SessionRuntime, StateError, TurnOutcome,
};
pub use vaults::{
    McpProbe, McpProbeStatus, McpRefreshBinding, TokenEndpointAuthBinding, VaultState, vault_router,
};
