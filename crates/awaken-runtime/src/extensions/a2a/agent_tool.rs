//! Unified agent delegation tool -- dispatches to local or remote backend.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::{
    BackendControl, BackendDelegatePolicy, BackendDelegateRunRequest, BackendParentContext,
    ExecutionBackend, execute_resolved_delegate_execution,
};
use crate::registry::{
    AgentResolver, ExecutionResolver, LocalExecutionResolver, ResolvedBackendAgent,
    ResolvedExecution,
};
use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::progress::ProgressStatus;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use super::a2a_backend::{A2aBackend, A2aConfig};
use super::progress_sink::ProgressForwardingSink;

/// Unified tool for agent delegation.
///
/// The LLM calls this tool to delegate work to a sub-agent. Routing to
/// local or remote backend is transparent -- determined at construction time.
pub struct AgentTool {
    /// Target agent ID.
    agent_id: String,
    /// Human-readable description for the LLM.
    description: String,
    /// Execution resolver used to build the canonical execution plan at call time.
    resolver: Arc<dyn ExecutionResolver>,
}

impl AgentTool {
    /// Create a tool that delegates to a local sub-agent.
    pub fn local(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self::with_execution_resolver(
            agent_id,
            description,
            Arc::new(LocalExecutionResolver::new(resolver)),
        )
    }

    /// Create a tool that delegates to a remote agent via A2A protocol.
    pub fn remote(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        config: A2aConfig,
    ) -> Self {
        let agent_id = agent_id.into();
        let description = description.into();
        Self::with_execution_resolver(
            agent_id.clone(),
            description.clone(),
            Arc::new(FixedExecutionResolver::non_local(
                &agent_id,
                &description,
                Arc::new(A2aBackend::new(config)),
            )),
        )
    }

    /// Create a tool with a custom execution backend.
    pub fn with_backend(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        backend: Arc<dyn ExecutionBackend>,
    ) -> Self {
        let agent_id = agent_id.into();
        let description = description.into();
        Self {
            agent_id: agent_id.clone(),
            description: description.clone(),
            resolver: Arc::new(FixedExecutionResolver::non_local(
                &agent_id,
                &description,
                backend,
            )),
        }
    }

    pub fn with_execution_resolver(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn ExecutionResolver>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            resolver,
        }
    }

    /// Returns the target agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        let tool_id = format!("agent_run_{}", self.agent_id);
        ToolDescriptor::new(&tool_id, &tool_id, &self.description).with_parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task to delegate to the sub-agent"
                }
            },
            "required": ["prompt"]
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        if args.get("prompt").and_then(Value::as_str).is_none() {
            return Err(ToolError::InvalidArguments(
                "missing required field \"prompt\"".into(),
            ));
        }
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        if prompt.is_empty() {
            return Err(ToolError::InvalidArguments(
                "prompt must not be empty".into(),
            ));
        }

        let tool_id = format!("agent_run_{}", self.agent_id);
        let messages = vec![awaken_contract::contract::message::Message::user(&prompt)];

        ctx.report_progress(
            ProgressStatus::Running,
            Some(&format!("delegating to {}", self.agent_id)),
            None,
        )
        .await;

        // Build a forwarding sink: if parent has a sink, filter through ProgressForwardingSink;
        // otherwise use NullEventSink
        let sink: Arc<dyn EventSink> = match &ctx.activity_sink {
            Some(parent_sink) => Arc::new(ProgressForwardingSink::new(parent_sink.clone())),
            None => Arc::new(NullEventSink),
        };

        let resolved = self
            .resolver
            .resolve_execution(&self.agent_id)
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        let request = BackendDelegateRunRequest {
            agent_id: &self.agent_id,
            new_messages: messages.clone(),
            messages,
            sink,
            resolver: self.resolver.as_ref(),
            parent: BackendParentContext {
                parent_run_id: Some(ctx.run_identity.run_id.clone()),
                parent_thread_id: Some(ctx.run_identity.thread_id.clone()),
                parent_tool_call_id: Some(ctx.call_id.clone()),
            },
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
        };

        let execution = execute_resolved_delegate_execution(&resolved, request).await;

        match execution {
            Ok(result) => {
                let progress_status = match result.status {
                    crate::backend::BackendRunStatus::Completed => ProgressStatus::Done,
                    crate::backend::BackendRunStatus::Cancelled => ProgressStatus::Cancelled,
                    crate::backend::BackendRunStatus::WaitingInput(_)
                    | crate::backend::BackendRunStatus::WaitingAuth(_)
                    | crate::backend::BackendRunStatus::Suspended(_)
                    | crate::backend::BackendRunStatus::Timeout
                    | crate::backend::BackendRunStatus::Failed(_) => ProgressStatus::Failed,
                };
                let progress_message = match &result.status {
                    crate::backend::BackendRunStatus::Completed => {
                        format!("delegation to {} completed", self.agent_id)
                    }
                    crate::backend::BackendRunStatus::Cancelled => {
                        format!("delegation to {} cancelled", self.agent_id)
                    }
                    crate::backend::BackendRunStatus::Failed(message) => {
                        format!("delegation to {} failed: {message}", self.agent_id)
                    }
                    crate::backend::BackendRunStatus::WaitingInput(message) => {
                        format!(
                            "delegation to {} waiting for input: {}",
                            self.agent_id,
                            message.as_deref().unwrap_or("input required")
                        )
                    }
                    crate::backend::BackendRunStatus::WaitingAuth(message) => {
                        format!(
                            "delegation to {} waiting for auth: {}",
                            self.agent_id,
                            message.as_deref().unwrap_or("auth required")
                        )
                    }
                    crate::backend::BackendRunStatus::Suspended(message) => {
                        format!(
                            "delegation to {} suspended: {}",
                            self.agent_id,
                            message.as_deref().unwrap_or("suspended")
                        )
                    }
                    crate::backend::BackendRunStatus::Timeout => {
                        format!("delegation to {} timed out", self.agent_id)
                    }
                };
                ctx.report_progress(progress_status, Some(&progress_message), None)
                    .await;

                let status_str = result.status.to_string();
                let mut tool_result = ToolResult::success(
                    &tool_id,
                    json!({
                        "agent_id": result.agent_id,
                        "status": status_str,
                        "response": result.response,
                        "output": result.output,
                        "steps": result.steps,
                    }),
                );
                if let Some(ref child_run_id) = result.run_id {
                    tool_result = tool_result.with_metadata(
                        "child_run_id",
                        serde_json::Value::String(child_run_id.clone()),
                    );
                }
                Ok(tool_result.into())
            }
            Err(error) => {
                ctx.report_progress(
                    ProgressStatus::Failed,
                    Some(&format!("delegation to {} failed: {error}", self.agent_id)),
                    None,
                )
                .await;
                Ok(ToolResult::error(&tool_id, error.to_string()).into())
            }
        }
    }
}

struct FixedExecutionResolver {
    execution: ResolvedExecution,
}

impl FixedExecutionResolver {
    fn non_local(agent_id: &str, description: &str, backend: Arc<dyn ExecutionBackend>) -> Self {
        let spec = Arc::new(awaken_contract::registry_spec::AgentSpec {
            id: agent_id.to_string(),
            model_id: String::new(),
            system_prompt: description.to_string(),
            ..Default::default()
        });
        Self {
            execution: ResolvedExecution::NonLocal(ResolvedBackendAgent::with_backend(
                spec, backend,
            )),
        }
    }
}

impl AgentResolver for FixedExecutionResolver {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        Err(crate::RuntimeError::ResolveFailed {
            message: format!("agent '{agent_id}' cannot be resolved locally"),
        })
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.execution.spec().id.clone()]
    }
}

impl ExecutionResolver for FixedExecutionResolver {
    fn resolve_execution(&self, agent_id: &str) -> Result<ResolvedExecution, crate::RuntimeError> {
        if self.execution.spec().id == agent_id {
            Ok(self.execution.clone())
        } else {
            Err(crate::RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            })
        }
    }
}
