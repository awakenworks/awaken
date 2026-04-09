//! In-process execution backend backed by the standard loop runner.

use async_trait::async_trait;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::TerminationReason;

use crate::loop_runner::{AgentLoopParams, prepare_resume, run_agent_loop};

use super::{
    BackendCapabilities, BackendRunRequest, BackendRunResult, BackendRunStatus, ExecutionBackend,
    ExecutionBackendError,
};

/// Local runtime backend for executing the standard loop in-process.
pub struct LocalBackend;

impl LocalBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionBackend for LocalBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::full()
    }

    async fn execute(
        &self,
        request: BackendRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let phase_runtime = if let Some(runtime) = request.phase_runtime {
            runtime
        } else {
            return self.execute_delegate(request).await;
        };

        let run_identity = request.run_identity.clone().ok_or_else(|| {
            ExecutionBackendError::ExecutionFailed(
                "local root execution requires run identity".into(),
            )
        })?;
        let run_id = run_identity.run_id.clone();
        if !request.decisions.is_empty() {
            prepare_resume(phase_runtime.store(), request.decisions, None)
                .map_err(crate::loop_runner::AgentLoopError::PhaseError)
                .map_err(ExecutionBackendError::Loop)?;
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: request.resolver,
            agent_id: request.agent_id,
            runtime: phase_runtime,
            sink: request.sink,
            checkpoint_store: request.checkpoint_store,
            messages: request.messages,
            run_identity,
            cancellation_token: request.control.cancellation_token,
            decision_rx: request.control.decision_rx,
            overrides: request.overrides,
            frontend_tools: request.frontend_tools,
            inbox: request.inbox,
            is_continuation: request.is_continuation,
        })
        .await
        .map_err(ExecutionBackendError::Loop)?;

        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            response: if result.response.is_empty() {
                None
            } else {
                Some(result.response)
            },
            steps: result.steps,
            run_id: Some(run_id),
            inbox: None,
        })
    }
}

impl LocalBackend {
    async fn execute_delegate(
        &self,
        request: BackendRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        request
            .resolver
            .resolve(request.agent_id)
            .map_err(|error| {
                ExecutionBackendError::AgentNotFound(format!(
                    "failed to resolve agent '{}': {error}",
                    request.agent_id
                ))
            })?;

        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        store
            .install_plugin(crate::loop_runner::LoopActionHandlersPlugin)
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        #[cfg(feature = "background")]
        let bg_manager = {
            let manager = crate::extensions::background::BackgroundTaskManager::new();
            let manager = std::sync::Arc::new(manager);
            manager.set_store(store.clone());
            Some(manager)
        };

        let phase_runtime = crate::phase::PhaseRuntime::new(store.clone())
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        if !request.decisions.is_empty() {
            prepare_resume(phase_runtime.store(), request.decisions, None)
                .map_err(crate::loop_runner::AgentLoopError::PhaseError)
                .map_err(ExecutionBackendError::Loop)?;
        }

        let (owner_inbox, inbox_receiver) = match request.inbox {
            Some(receiver) => (None, receiver),
            None => {
                let (sender, receiver) = crate::inbox::inbox_channel();
                (Some(sender), receiver)
            }
        };

        #[cfg(feature = "background")]
        if let Some(manager) = &bg_manager {
            if let Some(sender) = owner_inbox.clone() {
                manager.set_owner_inbox(sender);
            }
            store
                .install_plugin(crate::extensions::background::BackgroundTaskPlugin::new(
                    manager.clone(),
                ))
                .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        }

        let sub_run_id = uuid::Uuid::now_v7().to_string();
        let mut run_identity = RunIdentity::new(
            sub_run_id.clone(),
            request
                .parent
                .as_ref()
                .and_then(|parent| parent.parent_thread_id.clone()),
            sub_run_id.clone(),
            request
                .parent
                .as_ref()
                .and_then(|parent| parent.parent_run_id.clone()),
            request.agent_id.to_string(),
            RunOrigin::Subagent,
        );
        if let Some(parent_tool_call_id) = request
            .parent
            .as_ref()
            .and_then(|parent| parent.parent_tool_call_id.clone())
        {
            run_identity = run_identity.with_parent_tool_call_id(parent_tool_call_id);
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: request.resolver,
            agent_id: request.agent_id,
            runtime: &phase_runtime,
            sink: request.sink,
            checkpoint_store: None,
            messages: request.messages,
            run_identity,
            cancellation_token: request.control.cancellation_token,
            decision_rx: request.control.decision_rx,
            overrides: request.overrides,
            frontend_tools: request.frontend_tools,
            inbox: Some(inbox_receiver),
            is_continuation: false,
        })
        .await
        .map_err(ExecutionBackendError::Loop)?;

        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            response: if result.response.is_empty() {
                None
            } else {
                Some(result.response)
            },
            steps: result.steps,
            run_id: Some(sub_run_id),
            inbox: owner_inbox,
        })
    }
}

fn map_termination(termination: &TerminationReason) -> BackendRunStatus {
    match termination {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => {
            BackendRunStatus::Completed
        }
        TerminationReason::Cancelled => BackendRunStatus::Cancelled,
        TerminationReason::Stopped(reason) => {
            BackendRunStatus::Failed(format!("stopped: {reason:?}"))
        }
        TerminationReason::Blocked(message) => {
            BackendRunStatus::Failed(format!("blocked: {message}"))
        }
        TerminationReason::Suspended => BackendRunStatus::Failed("suspended".into()),
        TerminationReason::Error(message) => BackendRunStatus::Failed(message.clone()),
    }
}
