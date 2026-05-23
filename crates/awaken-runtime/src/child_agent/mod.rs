//! Plumbing for invoking a sub-agent run from inside a tool.
//!
//! [`run_child_agent`] is the single canonical entry point for spawning a
//! child agent run. Routes transparently to local or remote (A2A) backends
//! via [`ExecutionBackend`](crate::backend::ExecutionBackend), supports
//! parent → child state seeding, and returns the full [`BackendRunResult`]
//! so the calling tool can decode child output, propagate suspensions, or
//! read the child's final persisted state.
//!
//! State exchange between parent and child is the caller's responsibility:
//! - **Inbound**: build a [`PersistedState`] from parent state + tool args
//!   and pass via `initial_state_seed`.
//! - **Outbound**: read `BackendRunResult.state` after the call, then publish
//!   to parent state via `ToolOutput.command`.
//!
//! For tools that want to stream the child's tokens into their own output,
//! wrap the activity sink with [`StreamingPassthroughSink`].

pub mod sink;

pub use sink::StreamingPassthroughSink;

use std::sync::Arc;

use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::Message;
use awaken_contract::state::PersistedState;

use crate::backend::{
    BackendControl, BackendDelegatePolicy, BackendDelegateRunRequest, BackendParentContext,
    BackendRunResult, ExecutionBackendError, execute_resolved_delegate_execution,
};
use crate::registry::ExecutionResolver;

/// Parameters for [`run_child_agent`].
///
/// `messages` becomes both the full history and the new turn for the child
/// (delegate runs start fresh). `initial_state_seed` is applied to the child's
/// store after plugin activation but before the first inference step — see
/// [`StateStore::apply_seed`](crate::state::StateStore::apply_seed).
pub struct ChildAgentParams<'a> {
    pub resolver: &'a dyn ExecutionResolver,
    pub agent_id: &'a str,
    pub messages: Vec<Message>,
    pub parent: BackendParentContext,
    pub initial_state_seed: Option<PersistedState>,
    pub sink: Arc<dyn EventSink>,
    pub control: BackendControl,
    pub policy: BackendDelegatePolicy,
}

/// Spawn a child agent run and await its terminal state.
///
/// Returns the canonical [`BackendRunResult`] including final persisted state,
/// status, response, and any suspension reason. Callers decide how to map
/// these into a `ToolOutput` (typically packaging child state as a
/// `StateCommand` for the parent store).
pub async fn run_child_agent(
    params: ChildAgentParams<'_>,
) -> Result<BackendRunResult, ExecutionBackendError> {
    let ChildAgentParams {
        resolver,
        agent_id,
        messages,
        parent,
        initial_state_seed,
        sink,
        control,
        policy,
    } = params;

    let resolved = resolver
        .resolve_execution(agent_id)
        .map_err(|error| ExecutionBackendError::AgentNotFound(error.to_string()))?;

    let request = BackendDelegateRunRequest {
        agent_id,
        new_messages: messages.clone(),
        messages,
        sink,
        resolver,
        parent,
        control,
        policy,
        state_seed: initial_state_seed,
    };

    execute_resolved_delegate_execution(&resolved, request).await
}
