use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::validate_against_schema;

use super::validation::validate_a2ui_messages;
use super::{A2UI_TOOL_ID, A2UI_TOOL_NAME, MESSAGE_KEYS};

/// Tool for rendering A2UI declarative UI.
///
/// The LLM passes A2UI v0.8 message keys directly as tool arguments. The tool
/// schema stays intentionally message-level and defers component-shape details
/// to prompt guidance plus server-side validation. This keeps the function
/// signature small enough for real models to use reliably.
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

/// Normalize LLM-provided args to fix common structural mistakes.
///
/// Weaker models often flatten `surfaceUpdate`/`dataModelUpdate` fields to the
/// top level instead of nesting them. For example, they produce:
///   `{"surfaceUpdate": {...}, "surfaceId": "x", "components": [...]}`
/// instead of:
///   `{"surfaceUpdate": {"surfaceId": "x", "components": [...]}}`
///
/// This function detects the flattened pattern and rewraps it, so the tool
/// can accept the model's intent without a validation round-trip.
pub(crate) fn normalize_args(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };

    // Detect flattened surfaceUpdate: has "surfaceId" + "components" at top level
    // alongside a message key whose value is NOT the correct nested object.
    if obj.contains_key("surfaceId") && obj.contains_key("components") {
        // Find which message key is present (if any)
        let msg_key = MESSAGE_KEYS.iter().find(|k| obj.contains_key(**k)).copied();

        // Build the inner object from surfaceId + components + optional path
        let mut inner = serde_json::Map::new();
        if let Some(v) = obj.get("surfaceId") {
            inner.insert("surfaceId".into(), v.clone());
        }
        if let Some(v) = obj.get("components") {
            inner.insert("components".into(), v.clone());
        }
        if let Some(v) = obj.get("contents") {
            inner.insert("contents".into(), v.clone());
        }
        if let Some(v) = obj.get("path") {
            inner.insert("path".into(), v.clone());
        }
        if let Some(v) = obj.get("root") {
            inner.insert("root".into(), v.clone());
        }

        let key = msg_key.unwrap_or("surfaceUpdate");
        return json!({ key: inner });
    }

    args.clone()
}

/// Extract the A2UI messages array from LLM-provided args.
///
/// Accepts multiple input shapes:
/// 1. A2UI keys at top level → single-element array
///    `{"surfaceUpdate": {...}}` → `[{"surfaceUpdate": {...}}]`
/// 2. Legacy `messages` array → use directly
///    `{"messages": [{...}, {...}]}` → `[{...}, {...}]`
fn extract_messages(args: &Value) -> Vec<Value> {
    if let Some(arr) = args.get("messages").and_then(Value::as_array) {
        return arr.clone();
    }

    if let Some(obj) = args.as_object()
        && MESSAGE_KEYS.iter().any(|key| obj.contains_key(*key))
    {
        return vec![args.clone()];
    }

    vec![]
}

fn non_empty_string_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1
    })
}

fn component_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "component"],
        "properties": {
            "id": non_empty_string_schema(),
            "weight": { "type": "number" },
            "component": {
                "type": "object",
                "minProperties": 1,
                "maxProperties": 1,
                "additionalProperties": {
                    "type": "object"
                },
                "description": "A single official A2UI v0.8 component payload such as {\"Text\": {...}}."
            }
        }
    })
}

fn data_model_entry_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["key"],
        "properties": {
            "key": non_empty_string_schema(),
            "valueString": { "type": "string" },
            "valueNumber": { "type": "number" },
            "valueBoolean": { "type": "boolean" },
            "valueMap": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": true
                }
            }
        }
    })
}

fn surface_update_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["surfaceId", "components"],
        "properties": {
            "surfaceId": non_empty_string_schema(),
            "components": {
                "type": "array",
                "minItems": 1,
                "items": component_schema(),
                "description": "Flat component list. Every item must include id and component."
            }
        }
    })
}

fn data_model_update_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["surfaceId", "contents"],
        "properties": {
            "surfaceId": non_empty_string_schema(),
            "path": { "type": "string" },
            "contents": {
                "type": "array",
                "minItems": 1,
                "items": data_model_entry_schema()
            }
        }
    })
}

fn begin_rendering_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["surfaceId", "root"],
        "properties": {
            "surfaceId": non_empty_string_schema(),
            "root": non_empty_string_schema(),
            "styles": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            }
        }
    })
}

fn delete_surface_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["surfaceId"],
        "properties": {
            "surfaceId": non_empty_string_schema()
        }
    })
}

fn message_variant_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["surfaceUpdate"],
                "properties": { "surfaceUpdate": surface_update_schema() }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["dataModelUpdate"],
                "properties": { "dataModelUpdate": data_model_update_schema() }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["beginRendering"],
                "properties": { "beginRendering": begin_rendering_schema() }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["deleteSurface"],
                "properties": { "deleteSurface": delete_surface_schema() }
            }
        ]
    })
}

fn tool_parameters_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "minProperties": 1,
        "properties": {
            "surfaceUpdate": surface_update_schema(),
            "dataModelUpdate": data_model_update_schema(),
            "beginRendering": begin_rendering_schema(),
            "deleteSurface": delete_surface_schema(),
            "messages": {
                "type": "array",
                "minItems": 1,
                "items": message_variant_schema(),
                "description": "Optional batch format. Prefer the flat single-message shape above."
            }
        },
        "description": "Provide exactly one of surfaceUpdate, dataModelUpdate, beginRendering, deleteSurface, or messages."
    })
}

#[async_trait]
impl Tool for A2uiRenderTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            A2UI_TOOL_ID,
            A2UI_TOOL_NAME,
            "Sends A2UI v0.8 declarative UI to the client. Pass exactly one of \
             surfaceUpdate, dataModelUpdate, beginRendering, or deleteSurface \
             as a top-level key.",
        )
        .with_parameters(tool_parameters_schema())
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        let args = normalize_args(args);
        let messages = extract_messages(&args);

        if messages.is_empty() {
            return Err(ToolError::InvalidArguments(
                "expected at least one A2UI message key (surfaceUpdate, dataModelUpdate, beginRendering, or deleteSurface)"
                    .into(),
            ));
        }

        validate_against_schema(&tool_parameters_schema(), &args)?;

        let errors = validate_a2ui_messages(&messages);
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

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let args = normalize_args(&args);
        let messages = extract_messages(&args);

        tracing::debug!(
            count = messages.len(),
            "A2UI render tool: validated {} message(s)",
            messages.len()
        );

        Ok(ToolResult::success(
            A2UI_TOOL_NAME,
            json!({ "rendered": true, "count": messages.len() }),
        )
        .into())
    }
}
