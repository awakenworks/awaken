//! Local agent delegation backend -- executes a sub-agent in-process.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::Message;

use crate::loop_runner::{AgentLoopParams, run_agent_loop};
use crate::registry::AgentResolver;

use super::backend::{AgentBackend, AgentBackendError, DelegateRunResult, DelegateRunStatus};

/// Backend that delegates to a sub-agent running in the same process.
pub struct LocalBackend {
    resolver: Arc<dyn AgentResolver>,
}

impl LocalBackend {
    /// Create a new local backend with the given agent resolver.
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AgentBackend for LocalBackend {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        // Resolve the target agent
        self.resolver.resolve(agent_id).map_err(|e| {
            AgentBackendError::AgentNotFound(format!("failed to resolve agent '{agent_id}': {e}"))
        })?;

        // Build execution environment
        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(|e| AgentBackendError::ExecutionFailed(format!("state setup failed: {e}")))?;

        let phase_runtime = crate::phase::PhaseRuntime::new(store.clone())
            .map_err(|e| AgentBackendError::ExecutionFailed(format!("phase setup failed: {e}")))?;

        // Create sub-agent run identity
        let sub_run_id = uuid::Uuid::now_v7().to_string();
        let thread_id = sub_run_id.clone();
        let mut sub_identity = RunIdentity::new(
            thread_id.clone(),
            Some(thread_id),
            sub_run_id,
            parent_run_id,
            agent_id.to_string(),
            RunOrigin::Subagent,
        );
        if let Some(call_id) = parent_tool_call_id {
            sub_identity = sub_identity.with_parent_tool_call_id(call_id);
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: self.resolver.as_ref(),
            agent_id,
            runtime: &phase_runtime,
            sink: event_sink,
            checkpoint_store: None,
            messages,
            run_identity: sub_identity,
            cancellation_token: None,
            decision_rx: None,
            overrides: None,
            frontend_tools: Vec::new(),
        })
        .await
        .map_err(|e| {
            AgentBackendError::ExecutionFailed(format!(
                "sub-agent '{agent_id}' execution failed: {e}"
            ))
        })?;

        let status = match result.termination {
            TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => {
                DelegateRunStatus::Completed
            }
            TerminationReason::Cancelled => DelegateRunStatus::Cancelled,
            TerminationReason::Stopped(reason) => {
                DelegateRunStatus::Failed(format!("stopped: {reason:?}"))
            }
            TerminationReason::Blocked(msg) => DelegateRunStatus::Failed(format!("blocked: {msg}")),
            other => DelegateRunStatus::Failed(format!("{other:?}")),
        };

        let response = if result.response.is_empty() {
            None
        } else {
            Some(result.response)
        };

        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status,
            response,
            steps: result.steps,
        })
    }
}
