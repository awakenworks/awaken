use super::*;
use crate::runtime::background_tasks::{
    BackgroundExecutable, TaskResult as BgTaskResult, TaskStatus,
};
use schemars::JsonSchema;
use serde::Deserialize;

/// Arguments for the agent run tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentRunArgs {
    /// Target agent id (required for new runs).
    pub agent_id: Option<String>,
    /// Input for the target agent.
    pub prompt: Option<String>,
    /// Existing run id to resume or inspect.
    pub run_id: Option<String>,
    /// Whether to fork caller messages into the new run.
    #[serde(default)]
    pub fork_context: bool,
}

#[derive(Debug, Clone)]
pub struct AgentRunBackgroundPlan {
    agent_id: String,
    messages: Vec<Message>,
    initial_state: Option<Value>,
}

/// Normalize optional string: trim whitespace and treat empty as None.
fn normalize_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Task type used when registering sub-agent background runs.
pub(crate) const AGENT_RUN_TASK_TYPE: &str = "agent_run";

#[derive(Debug, Clone)]
pub struct AgentRunTool {
    os: AgentOs,
}

impl AgentRunTool {
    pub fn new(os: AgentOs) -> Self {
        Self { os }
    }

    fn ensure_target_visible(
        &self,
        target_agent_id: &str,
        caller_agent_id: Option<&str>,
        policy: Option<&tirea_contract::runtime::RunPolicy>,
    ) -> Result<(), ToolArgError> {
        if is_target_agent_visible(
            self.os.agents_registry().as_ref(),
            target_agent_id,
            caller_agent_id,
            policy,
        ) {
            return Ok(());
        }

        Err(ToolArgError::new(
            "unknown_agent",
            format!("Unknown or unavailable agent_id: {target_agent_id}"),
        ))
    }

    /// Resolve sub-agent inputs from args.
    /// Returns (agent_id, messages, initial_state).
    ///
    /// `prompt` is required for new runs but optional for resume (`is_resume`)
    /// scenarios where the sub-agent continues from its existing thread state.
    #[allow(clippy::result_large_err)]
    fn resolve_execution_context(
        &self,
        args: &AgentRunArgs,
        caller_messages: &[Arc<Message>],
        policy: Option<&tirea_contract::runtime::RunPolicy>,
        caller_agent_id: Option<&str>,
        is_resume: bool,
    ) -> Result<(String, Vec<Message>, Option<Value>), ToolResult> {
        let tool_name = AGENT_RUN_TOOL_ID;

        let agent_id = normalize_opt(args.agent_id.clone());
        let prompt = normalize_opt(args.prompt.clone());

        let Some(agent_id) = agent_id else {
            return Err(ToolArgError::new("invalid_arguments", "missing 'agent_id'")
                .into_tool_result(tool_name));
        };

        if !is_resume && prompt.is_none() {
            return Err(ToolArgError::new("invalid_arguments", "missing 'prompt'")
                .into_tool_result(tool_name));
        }

        if let Err(error) = self.ensure_target_visible(&agent_id, caller_agent_id, policy) {
            return Err(error.into_tool_result(tool_name));
        }

        let (messages, initial_state) = if args.fork_context {
            let mut msgs = filtered_fork_messages(
                caller_messages
                    .iter()
                    .map(|message| (**message).clone())
                    .collect(),
            );
            if let Some(prompt) = prompt {
                msgs.push(Message::user(prompt));
            }
            (msgs, None)
        } else if let Some(prompt) = prompt {
            (vec![Message::user(prompt)], None)
        } else {
            // Resume: no new prompt, continue from existing thread state.
            (Vec::new(), None)
        };

        Ok((agent_id, messages, initial_state))
    }

    fn normalize_background_args(args: &mut Value, agent_id: &str) -> Result<(), ToolResult> {
        let Some(obj) = args.as_object_mut() else {
            return Err(ToolResult::error(
                AGENT_RUN_TOOL_ID,
                "agent_run arguments must be a JSON object",
            ));
        };
        obj.insert("agent_id".to_string(), json!(agent_id));
        Ok(())
    }
}

#[async_trait]
impl BackgroundExecutable for AgentRunTool {
    type PreparedBackground = AgentRunBackgroundPlan;

    fn task_type(&self) -> &str {
        AGENT_RUN_TASK_TYPE
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn task_metadata(&self, args: &Value) -> Value {
        let run_id = args.get("run_id").and_then(Value::as_str).unwrap_or("");
        let agent_id = args
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        json!({
            "agent_id": agent_id,
            "thread_id": sub_agent_thread_id(run_id),
        })
    }

    fn task_id_from_args(&self, args: &Value) -> Option<String> {
        args.get("run_id")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn set_task_id_in_args(&self, args: &mut Value, task_id: &str) {
        if let Some(obj) = args.as_object_mut() {
            obj.insert("run_id".to_string(), json!(task_id));
        }
    }

    fn task_description(&self, args: &Value) -> String {
        let agent_id = args
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        format!("agent:{agent_id}")
    }

    fn prepare_background(
        &self,
        args: &mut Value,
        ctx: &ToolCallContext<'_>,
        is_resume: bool,
    ) -> Result<Self::PreparedBackground, ToolResult> {
        let parsed: AgentRunArgs = serde_json::from_value(args.clone()).map_err(|e| {
            ToolResult::error(
                AGENT_RUN_TOOL_ID,
                format!("invalid background agent_run args: {e}"),
            )
        })?;
        let caller_agent_id = ctx.caller_context().agent_id().map(str::to_string);
        let (agent_id, messages, initial_state) = self.resolve_execution_context(
            &parsed,
            ctx.caller_context().messages(),
            Some(ctx.run_policy()),
            caller_agent_id.as_deref(),
            is_resume,
        )?;
        Self::normalize_background_args(args, &agent_id)?;
        Ok(AgentRunBackgroundPlan {
            agent_id,
            messages,
            initial_state,
        })
    }

    fn foreground_task_status(&self, result: &ToolResult) -> (TaskStatus, Option<String>) {
        let status = result
            .data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let error = result
            .data
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| result.message.clone());

        match status {
            "failed" => (TaskStatus::Failed, error),
            "stopped" => (TaskStatus::Stopped, error),
            "cancelled" => (TaskStatus::Cancelled, error),
            _ => (TaskStatus::Completed, None),
        }
    }

    async fn execute_background(
        &self,
        task_id: &str,
        _args: Value,
        prepared: Self::PreparedBackground,
        execution: crate::runtime::background_tasks::BackgroundExecutionContext,
        cancel_token: RunCancellationToken,
    ) -> BgTaskResult {
        let request = SubAgentExecutionRequest {
            agent_id: prepared.agent_id,
            child_thread_id: sub_agent_thread_id(task_id),
            run_id: task_id.to_string(),
            parent_run_id: execution.parent_task_id,
            parent_tool_call_id: None,
            parent_thread_id: execution.owner_thread_id,
            messages: prepared.messages,
            initial_state: prepared.initial_state,
            cancellation_token: Some(cancel_token),
        };

        let completion = execute_sub_agent(self.os.clone(), request, None).await;

        match completion.status {
            SubAgentStatus::Completed => BgTaskResult::Success(json!({
                "run_id": task_id,
                "status": "completed"
            })),
            SubAgentStatus::Failed => BgTaskResult::Failed(completion.error.unwrap_or_default()),
            SubAgentStatus::Stopped => BgTaskResult::Stopped,
            SubAgentStatus::Running => BgTaskResult::Success(json!({ "run_id": task_id })),
        }
    }

    async fn execute_foreground(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
        is_resume: bool,
    ) -> Result<ToolResult, ToolError> {
        let parsed: AgentRunArgs = serde_json::from_value(args).map_err(|e| {
            ToolError::ExecutionFailed(format!("invalid arguments for agent_run: {e}"))
        })?;
        self.execute(parsed, ctx, is_resume).await
    }
}

#[async_trait]
impl crate::contracts::runtime::tool_call::TypedTool for AgentRunTool {
    type Args = AgentRunArgs;

    fn tool_id(&self) -> &str {
        AGENT_RUN_TOOL_ID
    }
    fn name(&self) -> &str {
        "Agent Run"
    }
    fn description(&self) -> &str {
        "Run or resume a registry agent; can run in background"
    }

    async fn execute(
        &self,
        args: AgentRunArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.execute(args, ctx, false).await
    }
}

impl AgentRunTool {
    async fn execute(
        &self,
        args: AgentRunArgs,
        ctx: &ToolCallContext<'_>,
        is_resume: bool,
    ) -> Result<ToolResult, ToolError> {
        let tool_name = AGENT_RUN_TOOL_ID;
        let owner_thread_id = ctx.caller_context().thread_id().map(str::to_string);
        let Some(owner_thread_id) = owner_thread_id else {
            return Ok(tool_error(
                tool_name,
                "missing_scope",
                "missing caller thread context",
            ));
        };
        let caller_agent_id = ctx.caller_context().agent_id().map(str::to_string);
        let caller_run_id = ctx
            .caller_context()
            .run_id()
            .map(str::to_string)
            .or_else(|| ctx.run_identity().run_id_opt().map(str::to_string));

        // run_id is always set (injected by BackgroundCapable wrapper for new tasks).
        let run_id = normalize_opt(args.run_id.clone()).unwrap_or_default();
        if run_id.is_empty() {
            return Ok(tool_error(
                tool_name,
                "missing_run_id",
                "run_id is required (should be injected by wrapper)",
            ));
        }

        // Resolve sub-agent inputs from args.
        let (agent_id, messages, initial_state) = match self.resolve_execution_context(
            &args,
            ctx.caller_context().messages(),
            Some(ctx.run_policy()),
            caller_agent_id.as_deref(),
            is_resume,
        ) {
            Ok(ctx) => ctx,
            Err(result) => return Ok(result),
        };

        let parent_tool_call_id = ctx.call_id().to_string();

        // Foreground execution with progress forwarding.
        let forward_progress =
            |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                ctx.report_tool_call_progress(update)
            };

        let request = SubAgentExecutionRequest {
            agent_id: agent_id.clone(),
            child_thread_id: sub_agent_thread_id(&run_id),
            run_id: run_id.clone(),
            parent_run_id: caller_run_id,
            parent_tool_call_id: Some(parent_tool_call_id),
            parent_thread_id: owner_thread_id,
            messages,
            initial_state,
            cancellation_token: None,
        };

        let completion = execute_sub_agent(self.os.clone(), request, Some(&forward_progress)).await;

        Ok(agent_tool_result(
            tool_name,
            &run_id,
            &agent_id,
            completion.status.as_str(),
            completion.error.as_deref(),
        ))
    }
}
