//! Plumbing for invoking a sub-agent run from inside a tool.
//!
//! [`run_child_agent`] is the single canonical entry point for spawning a
//! child agent run. It routes delegate execution through the resolved backend
//! ([`ExecutionBackend`](crate::backend::ExecutionBackend)), so local and
//! remote (A2A) children share one call shape, and returns the full
//! [`BackendRunResult`] so the calling tool can decode child output,
//! propagate suspensions, or read the child's final persisted state.
//!
//! State exchange between parent and child is the caller's responsibility:
//! - **Inbound**: build a [`PersistedState`] from parent state + tool args
//!   and pass via `initial_state_seed`.
//! - **Outbound**: read `BackendRunResult.state` after the call, then publish
//!   to parent state via `ToolOutput.command`.
//!
//! Backend capabilities still apply. Parent → child state seeding is
//! supported only by backends whose
//! The in-process Local backend applies the seed before the child's first
//! step. A2A and custom remote backends have no agreed seed-passing wire
//! protocol; `run_child_agent` rejects seeded delegate requests against
//! non-local execution plans rather than silently dropping the seed. If you
//! need to ship data to a remote child, encode it into the prompt yourself.
//!
//! For tools that want to stream the child's tokens into their own output,
//! wrap the activity sink with [`StreamingPassthroughSink`].

pub mod sink;

pub use sink::{ChildErrorForwarding, StreamingPassthroughSink};

use std::sync::Arc;

use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::Message;
use awaken_contract::state::PersistedState;

use crate::RuntimeError;
use crate::backend::{
    BackendControl, BackendDelegatePolicy, BackendDelegateRunRequest, BackendParentContext,
    BackendRunResult, BackendRunStatus, ExecutionBackendError, execute_resolved_delegate_execution,
};
use crate::cancellation::CancellationToken;
use crate::registry::AgentResolver;

/// Parameters for [`run_child_agent`].
///
/// Construct with [`Self::new`] instead of struct literal syntax. The type is
/// `#[non_exhaustive]` so future support for resumed child transcripts can
/// add explicit transcript/new-turn fields without breaking callers.
///
/// `initial_state_seed` is applied to the child's store after plugin
/// activation but before the first inference step — see
/// [`StateStore::apply_seed`](crate::state::StateStore::apply_seed).
#[non_exhaustive]
pub struct ChildAgentParams<'a> {
    pub resolver: &'a dyn AgentResolver,
    pub agent_id: &'a str,

    /// Initial conversation seed for a **fresh** delegate run.
    ///
    /// `run_child_agent` does not support resuming a prior delegate
    /// transcript: there is no full-history vs new-turn split. The vec you
    /// pass here is what the child sees as its starting input — typically
    /// a single [`Message::user`] built from your tool args. Internally it
    /// is forwarded as both `BackendDelegateRunRequest.messages` and
    /// `.new_messages`, mirroring how `AgentTool` invokes delegates.
    ///
    /// If a future revision needs to distinguish prior context from the
    /// new turn (continuation, multi-turn handoff), it will add dedicated
    /// input fields; do not rely on the current internal duplication to mean
    /// otherwise.
    pub initial_messages: Vec<Message>,

    pub parent: BackendParentContext,
    pub initial_state_seed: Option<PersistedState>,
    pub sink: Arc<dyn EventSink>,
    pub control: BackendControl,
    pub policy: BackendDelegatePolicy,
}

impl<'a> ChildAgentParams<'a> {
    /// Build parameters for a fresh child run.
    ///
    /// `initial_messages` is the child run's starting input. It is not a
    /// transcript/new-turn split and does not resume an existing delegate
    /// transcript.
    #[must_use]
    pub fn new(
        resolver: &'a dyn AgentResolver,
        agent_id: &'a str,
        initial_messages: Vec<Message>,
        parent: BackendParentContext,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            resolver,
            agent_id,
            initial_messages,
            parent,
            initial_state_seed: None,
            sink,
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
        }
    }

    /// Seed the child's state store before its first inference step.
    #[must_use]
    pub fn with_initial_state_seed(mut self, seed: PersistedState) -> Self {
        self.initial_state_seed = Some(seed);
        self
    }

    /// Override cooperative backend controls for the child run.
    #[must_use]
    pub fn with_control(mut self, control: BackendControl) -> Self {
        self.control = control;
        self
    }

    /// Propagate a parent cancellation token without rebuilding
    /// [`BackendControl`] by hand.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: Option<CancellationToken>) -> Self {
        self.control.cancellation_token = token;
        self
    }

    /// Override delegate execution policy for the child run.
    #[must_use]
    pub fn with_policy(mut self, policy: BackendDelegatePolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Error returned by [`run_child_agent_checked`].
#[derive(Debug, thiserror::Error)]
pub enum ChildAgentError {
    #[error(transparent)]
    Backend(#[from] ExecutionBackendError),
    #[error("child agent did not complete: {}", .0.status)]
    Terminal(Box<BackendRunResult>),
}

impl ChildAgentError {
    /// Return the terminal child result when the backend run finished in a
    /// non-success status.
    #[must_use]
    pub fn terminal_result(&self) -> Option<&BackendRunResult> {
        match self {
            Self::Backend(_) => None,
            Self::Terminal(result) => Some(result),
        }
    }
}

/// Spawn a child agent run and await its terminal state.
///
/// Returns the canonical [`BackendRunResult`] including final persisted state,
/// status, response, and any suspension reason. Callers decide how to map
/// these into a `ToolOutput` (typically packaging child state as a
/// `StateCommand` for the parent store).
///
/// This helper treats backend dispatch failures as `Err`, but it does **not**
/// treat a terminal child status such as `Failed`, `Cancelled`, `Timeout`, or
/// `Suspended` as an error. Callers must inspect [`BackendRunResult::status`]
/// before returning a successful parent tool result, or call
/// [`run_child_agent_checked`] when only `Completed` is acceptable.
pub async fn run_child_agent(
    params: ChildAgentParams<'_>,
) -> Result<BackendRunResult, ExecutionBackendError> {
    let ChildAgentParams {
        resolver,
        agent_id,
        initial_messages,
        parent,
        initial_state_seed,
        sink,
        control,
        policy,
    } = params;

    let resolved = resolver
        .resolve_execution(agent_id)
        .map_err(|error| map_resolve_error(agent_id, error))?;

    let request = BackendDelegateRunRequest {
        agent_id,
        new_messages: initial_messages.clone(),
        messages: initial_messages,
        sink,
        resolver,
        parent,
        control,
        policy,
        state_seed: initial_state_seed,
    };

    execute_resolved_delegate_execution(&resolved, request).await
}

fn map_resolve_error(agent_id: &str, error: RuntimeError) -> ExecutionBackendError {
    match error {
        RuntimeError::AgentNotFound { agent_id } => ExecutionBackendError::AgentNotFound(agent_id),
        other => ExecutionBackendError::ExecutionFailed(format!(
            "failed to resolve agent '{agent_id}': {other}"
        )),
    }
}

/// Spawn a child agent run and require it to complete successfully.
///
/// This is a convenience wrapper around [`run_child_agent`] for tools that
/// should fail when the child reaches any non-`Completed` terminal status.
pub async fn run_child_agent_checked(
    params: ChildAgentParams<'_>,
) -> Result<BackendRunResult, ChildAgentError> {
    let result = run_child_agent(params).await?;
    if matches!(result.status, BackendRunStatus::Completed) {
        Ok(result)
    } else {
        Err(ChildAgentError::Terminal(Box::new(result)))
    }
}
