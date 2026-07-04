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
mod state;
pub mod vaults;

pub use router::router;
pub use state::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, ManagedState, McpServerBinding,
    OutcomeIteration, OutcomeReport, Pending, RunError, RunErrorKind, SessionInit, SessionRuntime,
    StateError, TurnOutcome,
};
pub use vaults::{
    McpProbe, McpProbeStatus, McpRefreshBinding, TokenEndpointAuthBinding, VaultState, vault_router,
};
