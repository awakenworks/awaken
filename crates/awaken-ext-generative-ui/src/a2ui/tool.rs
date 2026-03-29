use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};

use super::validation::validate_a2ui_messages;
use super::{A2UI_TOOL_ID, A2UI_TOOL_NAME};

/// Tool for rendering A2UI declarative UI.
///
/// The LLM calls this tool with an array of A2UI messages (v0.9). The tool
/// validates the structural integrity and returns the validated payload.
pub struct A2uiRenderTool {
    _private: (),
}

impl A2uiRenderTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for A2uiRenderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for A2uiRenderTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            A2UI_TOOL_ID,
            A2UI_TOOL_NAME,
            "Sends A2UI JSON to the client to render declarative UI. \
             Each message must be a v0.9 A2UI object with exactly one of: \
             createSurface, updateComponents, updateDataModel, or deleteSurface.",
        )
        .with_parameters(serde_json::json!({
            "type": "object",
            "required": ["messages"],
            "properties": {
                "messages": {
                    "type": "array",
                    "description": "Array of A2UI v0.9 message objects. Each object must include \
                        \"version\": \"v0.9\" and exactly one of: createSurface, \
                        updateComponents, updateDataModel, or deleteSurface.",
                    "items": { "type": "object" }
                }
            }
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        let messages = args
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ToolError::InvalidArguments("missing required field \"messages\"".into())
            })?;

        if messages.is_empty() {
            return Err(ToolError::InvalidArguments(
                "messages array must not be empty".into(),
            ));
        }

        let errors = validate_a2ui_messages(messages);
        if errors.is_empty() {
            Ok(())
        } else {
            let details: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            Err(ToolError::InvalidArguments(format!(
                "A2UI validation failed: {}",
                details.join("; ")
            )))
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let messages = args
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        tracing::debug!(
            count = messages.len(),
            "A2UI render tool: validated {} message(s)",
            messages.len()
        );

        Ok(ToolResult::success(
            A2UI_TOOL_NAME,
            json!({
                "a2ui": messages,
                "rendered": true,
            }),
        ))
    }
}
