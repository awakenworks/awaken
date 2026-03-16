use super::actions::{deactivate_plan_mode_action, enter_plan_mode_action, exit_plan_mode_action};
use super::state::{PlanModeState, PlanRef};
use async_trait::async_trait;
use serde_json::{json, Value};
use tirea_contract::runtime::phase::AfterToolExecuteAction;
use tirea_contract::runtime::tool_call::context::ToolCallContext;
use tirea_contract::runtime::tool_call::tool::{
    Tool, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_state::State;

/// Tool to enter plan mode.
///
/// Activates read-only enforcement. Cannot be used in sub-agent contexts.
pub struct EnterPlanModeTool;

impl EnterPlanModeTool {
    /// Create a new enter plan mode tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnterPlanModeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "EnterPlanMode",
            "EnterPlanMode",
            "Enter plan mode to explore the codebase and create a structured plan \
             before making changes. In plan mode, only read-only tools are available.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
        .with_category("meta")
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        unreachable!("EnterPlanModeTool uses execute_effect")
    }

    async fn execute_effect(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        // Block sub-agents from entering plan mode
        if ctx.run_identity().agent_id_opt().is_some() {
            return Ok(ToolExecutionEffect::new(ToolResult::error(
                "EnterPlanMode",
                "Plan mode cannot be used in sub-agent contexts.",
            )));
        }

        // Check if already in plan mode
        let snapshot = ctx.snapshot();
        if let Ok(state) =
            PlanModeState::from_value(snapshot.get(PlanModeState::PATH).unwrap_or(&Value::Null))
        {
            if state.active {
                return Ok(ToolExecutionEffect::new(ToolResult::error(
                    "EnterPlanMode",
                    "Plan mode is already active.",
                )));
            }
        }

        let state_action = enter_plan_mode_action();

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "EnterPlanMode",
            json!({ "status": "plan_mode_activated" }),
        ))
        .with_action(AfterToolExecuteAction::State(state_action))
        .with_action(AfterToolExecuteAction::AddSystemReminder(
            "Plan mode is now active. Explore the codebase using read-only tools \
             (Read, Glob, Grep). When your plan is ready, call ExitPlanMode \
             with your plan content and a one-line summary."
                .to_string(),
        )))
    }
}

/// Tool to exit plan mode and submit the plan for approval.
///
/// The plan content is stored in the thread's state doc as a `PlanRef`.
/// The full plan text is returned in the tool result for immediate reference.
pub struct ExitPlanModeTool;

impl ExitPlanModeTool {
    /// Create a new exit plan mode tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ExitPlanModeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "ExitPlanMode",
            "ExitPlanMode",
            "Exit plan mode and submit your plan for approval. \
             Provide the full plan content and a one-line summary.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full plan content in markdown format."
                },
                "summary": {
                    "type": "string",
                    "description": "A one-line summary of the plan (used as a lightweight reference in future turns)."
                }
            },
            "required": ["plan", "summary"],
            "additionalProperties": false
        }))
        .with_category("meta")
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        unreachable!("ExitPlanModeTool uses execute_effect")
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let snapshot = ctx.snapshot();
        let state =
            PlanModeState::from_value(snapshot.get(PlanModeState::PATH).unwrap_or(&Value::Null))
                .unwrap_or_default();

        if !state.active {
            return Ok(ToolExecutionEffect::new(ToolResult::error(
                "ExitPlanMode",
                "Plan mode is not currently active.",
            )));
        }

        let plan_content = args
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        if plan_content.is_empty() {
            // No plan — just deactivate
            let state_action = deactivate_plan_mode_action();
            return Ok(ToolExecutionEffect::new(ToolResult::success_with_message(
                "ExitPlanMode",
                json!({ "status": "plan_mode_deactivated" }),
                "User has approved exiting plan mode. You can now proceed.",
            ))
            .with_action(AfterToolExecuteAction::State(state_action)));
        }

        let thread_id = ctx.run_identity().thread_id.clone();
        let plan_id = ctx.idempotency_key().to_string();

        let plan_ref = PlanRef {
            thread_id,
            plan_id: plan_id.clone(),
            summary: summary.to_string(),
        };

        let state_action = exit_plan_mode_action(plan_ref);

        let message = format!(
            "User has approved your plan. You can now start coding. \
             Start with updating your task list if applicable.\n\n\
             ## Approved Plan:\n{plan_content}"
        );

        Ok(ToolExecutionEffect::new(ToolResult::success_with_message(
            "ExitPlanMode",
            json!({
                "status": "plan_mode_deactivated",
                "plan_id": plan_id
            }),
            &message,
        ))
        .with_action(AfterToolExecuteAction::State(state_action)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_plan_mode_descriptor() {
        let tool = EnterPlanModeTool::new();
        let desc = tool.descriptor();
        assert_eq!(desc.name, "EnterPlanMode");
        assert_eq!(desc.category.as_deref(), Some("meta"));
    }

    #[test]
    fn exit_plan_mode_descriptor() {
        let tool = ExitPlanModeTool::new();
        let desc = tool.descriptor();
        assert_eq!(desc.name, "ExitPlanMode");
        assert_eq!(desc.category.as_deref(), Some("meta"));
        // Verify it requires plan and summary parameters
        let params = &desc.parameters;
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "plan"));
        assert!(required.iter().any(|v| v == "summary"));
    }
}
