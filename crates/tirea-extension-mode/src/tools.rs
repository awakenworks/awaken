use super::actions::request_mode_switch_action;
use super::state::AgentModeState;
use async_trait::async_trait;
use serde_json::{json, Value};
use tirea_contract::runtime::phase::AfterToolExecuteAction;
use tirea_contract::runtime::tool_call::context::ToolCallContext;
use tirea_contract::runtime::tool_call::tool::{
    Tool, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_state::State;

/// Tool to switch agent mode at runtime.
///
/// Emits a `RequestSwitch` state action. The actual re-resolution happens
/// when the `ModePlugin` detects the pending switch in `before_inference`
/// and terminates the run, allowing the `AgentOs::run()` loop to re-resolve.
pub struct SwitchModeTool;

impl SwitchModeTool {
    /// Create a new switch mode tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SwitchModeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SwitchModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "SwitchMode",
            "SwitchMode",
            "Switch the agent to a different named mode. Each mode is a configuration \
             profile that can override the model, system prompt, and other settings.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "description": "The name of the mode to switch to."
                }
            },
            "required": ["mode"],
            "additionalProperties": false
        }))
        .with_category("meta")
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        unreachable!("SwitchModeTool uses execute_effect")
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        if mode.is_empty() {
            return Ok(ToolExecutionEffect::new(ToolResult::error(
                "SwitchMode",
                "The 'mode' parameter must not be empty.",
            )));
        }

        // Check if already in the requested mode
        let snapshot = ctx.snapshot();
        if let Ok(state) =
            AgentModeState::from_value(snapshot.get(AgentModeState::PATH).unwrap_or(&Value::Null))
        {
            if state.active_mode.as_deref() == Some(mode) {
                return Ok(ToolExecutionEffect::new(ToolResult::success(
                    "SwitchMode",
                    json!({ "status": "already_active", "mode": mode }),
                )));
            }
        }

        let state_action = request_mode_switch_action(mode);

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "SwitchMode",
            json!({ "status": "mode_switch_requested", "mode": mode }),
        ))
        .with_action(AfterToolExecuteAction::State(state_action)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_mode_descriptor() {
        let tool = SwitchModeTool::new();
        let desc = tool.descriptor();
        assert_eq!(desc.name, "SwitchMode");
        assert_eq!(desc.category.as_deref(), Some("meta"));
        let required = desc.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "mode"));
    }
}
