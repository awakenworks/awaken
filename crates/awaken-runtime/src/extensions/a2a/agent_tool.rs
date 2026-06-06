//! Unified agent delegation tool -- dispatches to local or remote backend.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::ExecutionBackend;
use crate::delegation::{
    DelegateOutcome, DelegateParent, DelegateRequest, DelegateResult, DelegateRunner,
    ResolverDelegateRunner,
};
use crate::registry::{AgentResolver, ResolvedBackendAgent};
use crate::resolution::ExecutionPlan;
use awaken_runtime_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_runtime_contract::contract::progress::ProgressStatus;
use awaken_runtime_contract::contract::suspension::{
    PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
};
use awaken_runtime_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult, ToolStatus,
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
    /// Delegation mechanism (the narrow port). The tool never touches the
    /// execution backend directly — it declares intent through this runner.
    runner: Arc<dyn DelegateRunner>,
}

impl AgentTool {
    /// Create a tool that delegates to a local sub-agent.
    pub fn local(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self::with_execution_resolver(agent_id, description, resolver)
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
            Arc::new(FixedAgentResolver::non_local(
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
        Self::with_execution_resolver(
            agent_id.clone(),
            description.clone(),
            Arc::new(FixedAgentResolver::non_local(
                &agent_id,
                &description,
                backend,
            )),
        )
    }

    pub fn with_execution_resolver(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self::with_runner(
            agent_id,
            description,
            Arc::new(ResolverDelegateRunner::new(resolver)),
        )
    }

    /// Create a tool directly from a delegation runner (the narrow port).
    ///
    /// Crate-internal: the `DelegateRunner` seam is not part of the public
    /// facade (A-G10). Public construction goes through the resolver/backend
    /// constructors above.
    pub(crate) fn with_runner(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        runner: Arc<dyn DelegateRunner>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            description: description.into(),
            runner,
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
        let messages = vec![awaken_runtime_contract::contract::message::Message::user(
            &prompt,
        )];

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

        let request = DelegateRequest {
            agent_id: self.agent_id.clone(),
            messages,
            parent: DelegateParent {
                run_id: Some(ctx.run_identity.run_id.clone()),
                thread_id: Some(ctx.run_identity.thread_id.clone()),
                tool_call_id: Some(ctx.call_id.clone()),
            },
            sink,
        };

        match self.runner.run(request).await {
            Ok(result) => {
                let progress_status = match result.outcome {
                    DelegateOutcome::Completed => ProgressStatus::Done,
                    DelegateOutcome::Cancelled => ProgressStatus::Cancelled,
                    DelegateOutcome::WaitingInput
                    | DelegateOutcome::WaitingAuth
                    | DelegateOutcome::Suspended => ProgressStatus::Pending,
                    DelegateOutcome::Timeout | DelegateOutcome::Failed => ProgressStatus::Failed,
                };
                let progress_message = match result.outcome {
                    DelegateOutcome::Completed => {
                        format!("delegation to {} completed", self.agent_id)
                    }
                    DelegateOutcome::Cancelled => {
                        format!("delegation to {} cancelled", self.agent_id)
                    }
                    DelegateOutcome::Failed => {
                        format!(
                            "delegation to {} failed: {}",
                            self.agent_id,
                            result.status_message.as_deref().unwrap_or("failed")
                        )
                    }
                    DelegateOutcome::WaitingInput => {
                        format!(
                            "delegation to {} waiting for input: {}",
                            self.agent_id,
                            result.status_message.as_deref().unwrap_or("input required")
                        )
                    }
                    DelegateOutcome::WaitingAuth => {
                        format!(
                            "delegation to {} waiting for auth: {}",
                            self.agent_id,
                            result.status_message.as_deref().unwrap_or("auth required")
                        )
                    }
                    DelegateOutcome::Suspended => {
                        format!(
                            "delegation to {} suspended: {}",
                            self.agent_id,
                            result.status_message.as_deref().unwrap_or("suspended")
                        )
                    }
                    DelegateOutcome::Timeout => {
                        format!("delegation to {} timed out", self.agent_id)
                    }
                };
                ctx.report_progress(progress_status, Some(&progress_message), None)
                    .await;

                let child_run_id = result.child_run_id.clone();
                let mut tool_result =
                    tool_result_from_delegate(&tool_id, &result, progress_message, &args, ctx);
                if let Some(ref child_run_id) = child_run_id {
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

fn tool_result_from_delegate(
    tool_id: &str,
    result: &DelegateResult,
    message: String,
    args: &Value,
    ctx: &ToolCallContext,
) -> ToolResult {
    let payload = json!({
        "agent_id": result.agent_id.clone(),
        "status": result.status_label.clone(),
        "response": result.response.clone(),
        "output": result.output.clone(),
        "steps": result.steps,
    });

    match result.outcome {
        DelegateOutcome::Completed => ToolResult::success(tool_id, payload),
        DelegateOutcome::WaitingInput
        | DelegateOutcome::WaitingAuth
        | DelegateOutcome::Suspended => ToolResult {
            tool_name: tool_id.to_string(),
            status: ToolStatus::Pending,
            data: payload,
            message: Some(message),
            suspension: Some(Box::new(delegate_suspend_ticket(
                tool_id, result, args, ctx,
            ))),
            metadata: Default::default(),
        },
        DelegateOutcome::Cancelled | DelegateOutcome::Timeout | DelegateOutcome::Failed => {
            ToolResult {
                tool_name: tool_id.to_string(),
                status: ToolStatus::Error,
                data: payload,
                message: Some(message),
                suspension: None,
                metadata: Default::default(),
            }
        }
    }
}

fn delegate_suspend_ticket(
    tool_id: &str,
    result: &DelegateResult,
    args: &Value,
    ctx: &ToolCallContext,
) -> SuspendTicket {
    let (action, fallback_message) = match result.outcome {
        DelegateOutcome::WaitingInput => ("agent_delegate:input_required", "input required"),
        DelegateOutcome::WaitingAuth => ("agent_delegate:auth_required", "auth required"),
        DelegateOutcome::Suspended => ("agent_delegate:suspended", "suspended"),
        _ => ("agent_delegate:pending", "pending"),
    };
    let reason = result.status_message.as_deref().unwrap_or(fallback_message);
    let pending_id = if ctx.call_id.trim().is_empty() {
        tool_id.to_string()
    } else {
        ctx.call_id.clone()
    };
    let suspension_id = result
        .child_run_id
        .as_ref()
        .filter(|run_id| !run_id.trim().is_empty())
        .map(|run_id| format!("delegate_run:{run_id}"))
        .unwrap_or_else(|| format!("delegate_tool:{pending_id}"));
    SuspendTicket::new(
        Suspension {
            id: suspension_id,
            action: action.to_string(),
            message: reason.to_string(),
            parameters: json!({
                "agent_id": result.agent_id.clone(),
                "backend_status": result.status_label.clone(),
                "child_run_id": result.child_run_id.clone(),
                "tool_call_id": pending_id.clone(),
            }),
            response_schema: None,
        },
        PendingToolCall::new(pending_id, tool_id, args.clone()),
        ToolCallResumeMode::UseDecisionAsToolResult,
    )
}

struct FixedAgentResolver {
    execution: ExecutionPlan,
}

impl FixedAgentResolver {
    fn non_local(agent_id: &str, description: &str, backend: Arc<dyn ExecutionBackend>) -> Self {
        let spec = Arc::new(awaken_runtime_contract::registry_spec::AgentSpec {
            id: agent_id.to_string(),
            model_id: String::new(),
            system_prompt: description.to_string(),
            ..Default::default()
        });
        Self {
            execution: ExecutionPlan::Remote(ResolvedBackendAgent::with_backend(spec, backend)),
        }
    }
}

impl AgentResolver for FixedAgentResolver {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        Err(crate::RuntimeError::ResolveFailed {
            message: format!("agent '{agent_id}' cannot be resolved locally"),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, crate::RuntimeError> {
        if self.execution.spec().id == agent_id {
            Ok(self.execution.clone())
        } else {
            Err(crate::RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            })
        }
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.execution.spec().id.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{DelegateError, DelegateOutcome, DelegateRequest, DelegateResult};
    use awaken_runtime_contract::contract::identity::{RunIdentity, RunOrigin};
    use awaken_runtime_contract::registry_spec::AgentSpec;

    /// A `DelegateRunner` that returns a fixed outcome, so we can assert the
    /// tool's `DelegateOutcome` → `ToolResult`/suspension mapping in isolation.
    struct MockRunner {
        outcome: DelegateOutcome,
        status_label: &'static str,
        status_message: Option<String>,
        child_run_id: Option<String>,
    }

    #[async_trait]
    impl DelegateRunner for MockRunner {
        async fn run(&self, _request: DelegateRequest) -> Result<DelegateResult, DelegateError> {
            Ok(DelegateResult {
                outcome: self.outcome,
                status_label: self.status_label.to_string(),
                status_message: self.status_message.clone(),
                agent_id: "child".into(),
                response: Some("ok".into()),
                output: Value::Null,
                steps: 1,
                child_run_id: self.child_run_id.clone(),
            })
        }
    }

    fn ctx() -> ToolCallContext {
        ToolCallContext {
            call_id: "call-1".into(),
            tool_name: "agent_run_child".into(),
            run_identity: RunIdentity::new(
                "thread-1".into(),
                None,
                "run-1".into(),
                None,
                "parent".into(),
                RunOrigin::User,
            ),
            agent_spec: Arc::new(AgentSpec::default()),
            snapshot: crate::state::StateStore::new().snapshot(),
            activity_sink: None,
            cancellation_token: None,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
        }
    }

    fn tool(outcome: DelegateOutcome, label: &'static str, msg: Option<&str>) -> AgentTool {
        AgentTool::with_runner(
            "child",
            "desc",
            Arc::new(MockRunner {
                outcome,
                status_label: label,
                status_message: msg.map(str::to_string),
                child_run_id: Some("run-7".into()),
            }),
        )
    }

    #[tokio::test]
    async fn completed_maps_to_success() {
        let out = tool(DelegateOutcome::Completed, "completed", None)
            .execute(json!({"prompt": "hi"}), &ctx())
            .await
            .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Success));
        assert_eq!(out.result.data["status"], "completed");
        assert!(out.result.suspension.is_none());
    }

    #[tokio::test]
    async fn failed_maps_to_error() {
        let out = tool(DelegateOutcome::Failed, "failed", Some("boom"))
            .execute(json!({"prompt": "hi"}), &ctx())
            .await
            .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Error));
        assert!(out.result.suspension.is_none());
    }

    #[tokio::test]
    async fn waiting_input_maps_to_pending_suspension() {
        let out = tool(
            DelegateOutcome::WaitingInput,
            "waiting_input",
            Some("need x"),
        )
        .execute(json!({"prompt": "hi"}), &ctx())
        .await
        .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Pending));
        let ticket = out.result.suspension.as_ref().expect("suspension ticket");
        assert_eq!(ticket.suspension.action, "agent_delegate:input_required");
        assert_eq!(ticket.suspension.id, "delegate_run:run-7");
    }

    #[tokio::test]
    async fn suspended_maps_to_pending_suspension() {
        let out = tool(DelegateOutcome::Suspended, "suspended", None)
            .execute(json!({"prompt": "hi"}), &ctx())
            .await
            .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Pending));
        let ticket = out.result.suspension.as_ref().expect("suspension ticket");
        assert_eq!(ticket.suspension.action, "agent_delegate:suspended");
    }

    #[tokio::test]
    async fn cancelled_maps_to_error() {
        let out = tool(DelegateOutcome::Cancelled, "cancelled", None)
            .execute(json!({"prompt": "hi"}), &ctx())
            .await
            .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Error));
        assert!(out.result.suspension.is_none());
    }

    #[tokio::test]
    async fn timeout_maps_to_error() {
        let out = tool(DelegateOutcome::Timeout, "timeout", None)
            .execute(json!({"prompt": "hi"}), &ctx())
            .await
            .unwrap();
        assert!(matches!(out.result.status, ToolStatus::Error));
        assert!(out.result.suspension.is_none());
    }
}
