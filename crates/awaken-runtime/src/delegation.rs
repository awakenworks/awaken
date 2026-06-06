//! Internal delegation port — the seam between delegation *tools* (UI) and the
//! delegation *mechanism* (resolver + execution backend).
//!
//! [`AgentTool`](crate::extensions::a2a::AgentTool) and other delegation tools
//! depend only on [`DelegateRunner`] and the backend-agnostic [`DelegateResult`]
//! DTO; they never touch `backend::*` / `registry::*` / `resolution::*` on the
//! hot path. [`ResolverDelegateRunner`] is the one place that maps the runtime's
//! `BackendRunStatus`/`BackendRunResult` onto these DTOs.
//!
//! This is the in-crate precursor to a published `DelegateRunner` contract port:
//! once the boundary proves out here, the trait + DTOs can move to
//! `awaken-runtime-contract` and the tools can move to their own crate.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use awaken_runtime_contract::contract::event_sink::EventSink;
use awaken_runtime_contract::contract::message::Message;

use crate::backend::{
    BackendControl, BackendDelegatePolicy, BackendDelegateRunRequest, BackendParentContext,
    BackendRunResult, BackendRunStatus, execute_resolved_delegate_execution,
};
use crate::registry::AgentResolver;

/// Parent lineage for a delegated child run.
#[derive(Debug, Clone, Default)]
pub struct DelegateParent {
    pub run_id: Option<String>,
    pub thread_id: Option<String>,
    pub tool_call_id: Option<String>,
}

/// A request to run a child agent on behalf of a delegating tool.
pub struct DelegateRequest {
    pub agent_id: String,
    pub messages: Vec<Message>,
    pub parent: DelegateParent,
    pub sink: Arc<dyn EventSink>,
}

/// Backend-agnostic terminal/waiting state of a delegated child run.
///
/// Mirrors the control-flow shape of `BackendRunStatus` without exposing the
/// backend type to tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateOutcome {
    Completed,
    Cancelled,
    Timeout,
    Failed,
    WaitingInput,
    WaitingAuth,
    Suspended,
}

/// Backend-agnostic result of a delegated child run.
pub struct DelegateResult {
    pub outcome: DelegateOutcome,
    /// Stable status label, identical to `BackendRunStatus::to_string()`, kept
    /// for wire/suspension compatibility.
    pub status_label: String,
    /// Inner message for waiting/suspended/failed states, if any.
    pub status_message: Option<String>,
    pub agent_id: String,
    pub response: Option<String>,
    /// Child output, pre-serialized so tools need not know the backend type.
    pub output: Value,
    pub steps: usize,
    pub child_run_id: Option<String>,
}

/// Error surfaced when delegation cannot run (resolution or backend failure).
#[derive(Debug, Clone)]
pub struct DelegateError(pub String);

impl std::fmt::Display for DelegateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DelegateError {}

/// The delegation mechanism, as seen by tools. Resolves a target agent and runs
/// it to a terminal/waiting state, returning a backend-agnostic result.
#[async_trait]
pub trait DelegateRunner: Send + Sync {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateResult, DelegateError>;
}

/// Default mechanism: resolve via an [`AgentResolver`] and dispatch through the
/// execution backend. This is the only code that depends on `BackendRunStatus`.
pub struct ResolverDelegateRunner {
    resolver: Arc<dyn AgentResolver>,
}

impl ResolverDelegateRunner {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl DelegateRunner for ResolverDelegateRunner {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateResult, DelegateError> {
        let resolved = self
            .resolver
            .resolve_execution(&request.agent_id)
            .map_err(|error| DelegateError(error.to_string()))?;

        let backend_request = BackendDelegateRunRequest {
            agent_id: &request.agent_id,
            new_messages: request.messages.clone(),
            messages: request.messages,
            sink: request.sink,
            resolver: self.resolver.as_ref(),
            parent: BackendParentContext {
                parent_run_id: request.parent.run_id,
                parent_thread_id: request.parent.thread_id,
                parent_tool_call_id: request.parent.tool_call_id,
            },
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
            state_seed: None,
        };

        let result = execute_resolved_delegate_execution(&resolved, backend_request)
            .await
            .map_err(|error| DelegateError(error.to_string()))?;

        Ok(map_backend_result(result))
    }
}

/// Map a `BackendRunResult` onto the backend-agnostic [`DelegateResult`].
fn map_backend_result(result: BackendRunResult) -> DelegateResult {
    let status_label = result.status.to_string();
    let (outcome, status_message) = match result.status {
        BackendRunStatus::Completed => (DelegateOutcome::Completed, None),
        BackendRunStatus::Cancelled => (DelegateOutcome::Cancelled, None),
        BackendRunStatus::Timeout => (DelegateOutcome::Timeout, None),
        BackendRunStatus::Failed(message) => (DelegateOutcome::Failed, Some(message)),
        BackendRunStatus::WaitingInput(message) => (DelegateOutcome::WaitingInput, message),
        BackendRunStatus::WaitingAuth(message) => (DelegateOutcome::WaitingAuth, message),
        BackendRunStatus::Suspended(message) => (DelegateOutcome::Suspended, message),
    };

    DelegateResult {
        outcome,
        status_label,
        status_message,
        agent_id: result.agent_id,
        response: result.response,
        output: serde_json::to_value(&result.output).unwrap_or(Value::Null),
        steps: result.steps,
        child_run_id: result.run_id,
    }
}
