//! A2UI render tool.
//!
//! Allows the LLM to send A2UI declarative UI messages to the client.
//! The tool validates the payload and returns the validated JSON as its result,
//! which flows through the AG-UI event stream (`TOOL_CALL_RESULT`) to the
//! frontend for rendering.
//!
//! This mirrors Google's `send_a2ui_json_to_client` tool from the A2UI SDK.

use crate::validate::validate_a2ui_messages;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea_contract::runtime::tool_call::{ToolCallContext, ToolError, ToolResult, TypedTool};
use tracing::debug;

const TOOL_ID: &str = "render_a2ui";
const TOOL_NAME: &str = "render_a2ui";

/// Tool for rendering A2UI declarative UI.
///
/// The LLM calls this tool with an array of A2UI messages (v0.9). The tool
/// validates the structural integrity of each message and returns the validated
/// payload. The frontend identifies results from this tool and routes them to
/// its A2UI renderer.
///
/// # Example tool call
///
/// ```json
/// {
///   "messages": [
///     {
///       "version": "v0.9",
///       "createSurface": {
///         "surfaceId": "contact_form",
///         "catalogId": "https://a2ui.org/specification/v0_9/basic_catalog.json"
///       }
///     },
///     {
///       "version": "v0.9",
///       "updateComponents": {
///         "surfaceId": "contact_form",
///         "components": [
///           {"id": "root", "component": "Card", "child": "col"},
///           {"id": "col", "component": "Column", "children": ["name_field"]},
///           {"id": "name_field", "component": "TextField", "label": "Name"}
///         ]
///       }
///     }
///   ]
/// }
/// ```
pub struct A2uiRenderTool {
    _private: (),
}

impl A2uiRenderTool {
    /// Create a new A2UI render tool.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for A2uiRenderTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A single A2UI v0.9 message.
///
/// Must contain `"version": "v0.9"` and exactly one of:
/// `createSurface`, `updateComponents`, `updateDataModel`, or `deleteSurface`.
/// Additional fields are passed through to the renderer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiMessage {
    /// Protocol version, must be "v0.9".
    pub version: String,
    /// Create a new surface (mutually exclusive with other message types).
    #[serde(
        rename = "createSurface",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub create_surface: Option<A2uiCreateSurface>,
    /// Update components on an existing surface.
    #[serde(
        rename = "updateComponents",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub update_components: Option<A2uiUpdateComponents>,
    /// Update the data model of a surface.
    #[serde(
        rename = "updateDataModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub update_data_model: Option<A2uiUpdateDataModel>,
    /// Delete a surface.
    #[serde(
        rename = "deleteSurface",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub delete_surface: Option<A2uiDeleteSurface>,
}

/// Parameters for creating a new A2UI surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiCreateSurface {
    /// Unique identifier for this surface.
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    /// Catalog URL or identifier for available components.
    #[serde(rename = "catalogId")]
    pub catalog_id: String,
}

/// Parameters for updating components on a surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiUpdateComponents {
    /// Target surface identifier.
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    /// Array of component definitions to render.
    pub components: Vec<A2uiComponent>,
}

/// A single A2UI component definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiComponent {
    /// Unique component identifier within the surface.
    pub id: String,
    /// Component type name from the catalog (e.g. "Card", "TextField", "Button").
    pub component: String,
    /// Optional single child component ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<String>,
    /// Optional ordered child component IDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<String>>,
    /// Display text or label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Display label for form fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Chart/data field configurations and other component-specific properties.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

/// Parameters for updating the data model of a surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiUpdateDataModel {
    /// Target surface identifier.
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    /// JSON path within the data model to update.
    pub path: String,
    /// New value at the given path.
    pub value: Value,
}

/// Parameters for deleting a surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2uiDeleteSurface {
    /// Surface identifier to delete.
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
}

/// Arguments for the A2UI render tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A2uiRenderArgs {
    /// Array of A2UI v0.9 messages to send to the client.
    pub messages: Vec<A2uiMessage>,
}

#[async_trait]
impl TypedTool for A2uiRenderTool {
    type Args = A2uiRenderArgs;

    fn tool_id(&self) -> &str {
        TOOL_ID
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Sends A2UI JSON to the client to render declarative UI. \
         Each message must be a v0.9 A2UI object with exactly one of: \
         createSurface, updateComponents, updateDataModel, or deleteSurface. \
         The messages array is sent to the frontend for rendering."
    }

    fn validate(&self, args: &Self::Args) -> Result<(), String> {
        if args.messages.is_empty() {
            return Err("messages array must not be empty".to_string());
        }
        let values = messages_to_values(&args.messages);
        let errors = validate_a2ui_messages(&values);
        if errors.is_empty() {
            Ok(())
        } else {
            let details: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            Err(format!("A2UI validation failed: {}", details.join("; ")))
        }
    }

    async fn execute(
        &self,
        args: Self::Args,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        debug!(
            count = args.messages.len(),
            "A2UI render tool: validated {} message(s)",
            args.messages.len()
        );

        let values = messages_to_values(&args.messages);
        Ok(ToolResult::success(
            TOOL_NAME,
            json!({
                "a2ui": values,
                "rendered": true,
            }),
        ))
    }
}

fn messages_to_values(messages: &[A2uiMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::testing::TestFixture;

    fn contact_form_messages() -> Vec<A2uiMessage> {
        serde_json::from_value(json!([
            {
                "version": "v0.9",
                "createSurface": {
                    "surfaceId": "form_1",
                    "catalogId": "https://a2ui.org/specification/v0_9/basic_catalog.json"
                }
            },
            {
                "version": "v0.9",
                "updateComponents": {
                    "surfaceId": "form_1",
                    "components": [
                        {"id": "root", "component": "Card", "child": "col"},
                        {"id": "col", "component": "Column", "children": ["title", "name_field", "btn"]},
                        {"id": "title", "component": "Text", "text": "Contact Form", "variant": "h2"},
                        {"id": "name_field", "component": "TextField", "label": "Name", "value": {"path": "/contact/name"}},
                        {"id": "btn", "component": "Button", "text": "Submit", "action": {"event": {"name": "submit"}}}
                    ]
                }
            },
            {
                "version": "v0.9",
                "updateDataModel": {
                    "surfaceId": "form_1",
                    "path": "/contact",
                    "value": {"name": ""}
                }
            }
        ])).expect("valid test messages")
    }

    #[test]
    fn descriptor_has_correct_id_and_name() {
        use tirea_contract::runtime::tool_call::Tool;
        let tool = A2uiRenderTool::new();
        let desc = tool.descriptor();
        assert_eq!(desc.id, TOOL_ID);
        assert_eq!(desc.name, TOOL_NAME);
        assert!(desc.description.contains("A2UI"));
    }

    #[test]
    fn validate_accepts_valid_messages() {
        let tool = A2uiRenderTool::new();
        let args = A2uiRenderArgs {
            messages: contact_form_messages(),
        };
        assert!(tool.validate(&args).is_ok());
    }

    #[test]
    fn validate_rejects_empty_messages() {
        let tool = A2uiRenderTool::new();
        let args = A2uiRenderArgs { messages: vec![] };
        let err = tool.validate(&args).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn validate_rejects_invalid_a2ui() {
        let tool = A2uiRenderTool::new();
        let args = A2uiRenderArgs {
            messages: vec![A2uiMessage {
                version: "v0.9".to_string(),
                create_surface: None,
                update_components: None,
                update_data_model: None,
                delete_surface: None,
            }],
        };
        let err = tool.validate(&args).unwrap_err();
        assert!(err.contains("missing message type"));
    }

    #[tokio::test]
    async fn execute_returns_validated_payload() {
        let tool = A2uiRenderTool::new();
        let args = A2uiRenderArgs {
            messages: contact_form_messages(),
        };
        let fixture = TestFixture::new();
        let result = TypedTool::execute(&tool, args, &fixture.ctx())
            .await
            .unwrap();
        assert_eq!(result.tool_name, TOOL_NAME);
        assert_eq!(result.data["rendered"], true);
        assert!(result.data["a2ui"].is_array());
        assert_eq!(result.data["a2ui"].as_array().unwrap().len(), 3);
    }
}
