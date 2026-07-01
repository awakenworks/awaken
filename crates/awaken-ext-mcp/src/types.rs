//! Prompt and resource value types, projected from the MCP wire shapes.
//!
//! These mirror the reference crates' definitions so the same server replies
//! deserialize identically. They are plain data — the transport parses wire
//! JSON into them and hands them to the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One declared argument of a prompt (`prompts/list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// A prompt a server exposes (`prompts/list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// One message of a rendered prompt (`prompts/get`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Value,
}

/// The rendered result of `prompts/get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

/// A resource a server exposes (`resources/list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceDefinition {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Internal envelope for a `prompts/list` reply.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListPromptsResult {
    #[serde(default)]
    pub(crate) prompts: Vec<McpPromptDefinition>,
}

/// Internal envelope for a `resources/list` reply.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListResourcesResult {
    #[serde(default)]
    pub(crate) resources: Vec<McpResourceDefinition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_list_deserializes() {
        let value = serde_json::json!({
            "prompts": [
                { "name": "greet", "description": "say hi",
                  "arguments": [{ "name": "who", "required": true }] },
                { "name": "bare" }
            ]
        });
        let parsed: ListPromptsResult = serde_json::from_value(value).expect("parses");
        assert_eq!(parsed.prompts.len(), 2);
        assert_eq!(parsed.prompts[0].name, "greet");
        assert_eq!(parsed.prompts[0].arguments[0].name, "who");
        assert!(parsed.prompts[0].arguments[0].required);
        // A prompt with no arguments defaults to an empty list, not an error.
        assert!(parsed.prompts[1].arguments.is_empty());
    }

    #[test]
    fn prompt_get_result_deserializes() {
        let value = serde_json::json!({
            "description": "a greeting",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "hi" } }
            ]
        });
        let parsed: McpPromptResult = serde_json::from_value(value).expect("parses");
        assert_eq!(parsed.description.as_deref(), Some("a greeting"));
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].role, "user");
    }

    #[test]
    fn resources_list_deserializes_with_mime_rename() {
        let value = serde_json::json!({
            "resources": [
                { "uri": "file:///a.txt", "name": "a", "mimeType": "text/plain", "size": 12 }
            ]
        });
        let parsed: ListResourcesResult = serde_json::from_value(value).expect("parses");
        assert_eq!(parsed.resources.len(), 1);
        assert_eq!(parsed.resources[0].uri, "file:///a.txt");
        assert_eq!(parsed.resources[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(parsed.resources[0].size, Some(12));
    }

    #[test]
    fn empty_list_replies_deserialize_to_empty() {
        let prompts: ListPromptsResult =
            serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(prompts.prompts.is_empty());
        let resources: ListResourcesResult =
            serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(resources.resources.is_empty());
    }
}
