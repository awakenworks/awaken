//! Sampling: server->client `sampling/createMessage`.
//!
//! An MCP server may ask the *client* to run a model completion. The runtime
//! owns the model, not this crate — so a host implements [`SamplingHandler`]
//! against neutral request/response types, and [`SamplingBridge`] adapts it to
//! the wire as a [`ServerRequestHandler`](crate::jsonrpc::ServerRequestHandler).
//! No `mcp` wire types cross to the host (anti-corruption), and no kernel
//! dependency is pulled in.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::jsonrpc::{ServerRequestError, ServerRequestHandler};

/// One message in a sampling request: a role and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingMessage {
    pub role: String,
    pub content: String,
}

/// A neutral sampling request handed to the host's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingRequest {
    pub messages: Vec<SamplingMessage>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u64>,
}

/// The host's model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingResponse {
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
}

impl SamplingResponse {
    /// An assistant reply carrying `content`.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            model: None,
            stop_reason: None,
        }
    }
}

/// The host could not produce a sampling reply; becomes a JSON-RPC error.
#[derive(Debug, Clone)]
pub struct SamplingError(pub String);

/// Runs one model completion for a server-initiated `sampling/createMessage`.
/// The host implements this over the runtime's model, keeping this crate free of
/// a kernel dependency (the same inversion `awaken-ext-goal` uses for its judge).
#[async_trait]
pub trait SamplingHandler: Send + Sync {
    async fn create_message(
        &self,
        request: SamplingRequest,
    ) -> Result<SamplingResponse, SamplingError>;
}

/// The MCP method name for a sampling request.
pub const SAMPLING_METHOD: &str = "sampling/createMessage";

/// Adapts a [`SamplingHandler`] into a [`ServerRequestHandler`]: it answers
/// `sampling/createMessage` and rejects any other server request.
pub struct SamplingBridge {
    handler: Arc<dyn SamplingHandler>,
}

impl SamplingBridge {
    pub fn new(handler: Arc<dyn SamplingHandler>) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl ServerRequestHandler for SamplingBridge {
    async fn handle(
        &self,
        _id: &Value,
        method: &str,
        params: Value,
    ) -> Result<Value, ServerRequestError> {
        if method != SAMPLING_METHOD {
            return Err(ServerRequestError::method_not_found(method));
        }
        let request = parse_sampling_request(&params);
        match self.handler.create_message(request).await {
            Ok(response) => Ok(build_sampling_result(&response)),
            // Sampling failure maps to a JSON-RPC internal error (-32603).
            Err(SamplingError(message)) => Err(ServerRequestError {
                code: -32603,
                message,
            }),
        }
    }
}

/// Parse `sampling/createMessage` params into a neutral [`SamplingRequest`].
/// Non-text content blocks are skipped (only text sampling is supported).
pub fn parse_sampling_request(params: &Value) -> SamplingRequest {
    let messages = params
        .get("messages")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| SamplingMessage {
                    role: m
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("user")
                        .to_string(),
                    content: extract_text(m.get("content").unwrap_or(&Value::Null)),
                })
                .collect()
        })
        .unwrap_or_default();
    SamplingRequest {
        messages,
        system_prompt: params
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        max_tokens: params.get("maxTokens").and_then(Value::as_u64),
    }
}

/// Extract text from a content value that may be a bare string, a single
/// `{type:"text",text}` block, or an array of such blocks.
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Build a `CreateMessageResult` wire value from a neutral response.
pub fn build_sampling_result(response: &SamplingResponse) -> Value {
    let mut result = json!({
        "role": response.role,
        "content": { "type": "text", "text": response.content },
    });
    if let Some(model) = &response.model {
        result["model"] = json!(model);
    }
    if let Some(stop) = &response.stop_reason {
        result["stopReason"] = json!(stop);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_messages_system_and_max_tokens() {
        let params = json!({
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "hello" } },
                { "role": "assistant", "content": "hi there" }
            ],
            "systemPrompt": "be nice",
            "maxTokens": 128
        });
        let request = parse_sampling_request(&params);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content, "hello");
        assert_eq!(request.messages[1].content, "hi there");
        assert_eq!(request.system_prompt.as_deref(), Some("be nice"));
        assert_eq!(request.max_tokens, Some(128));
    }

    #[test]
    fn parse_joins_array_content_blocks() {
        let params = json!({
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "a" },
                    { "type": "text", "text": "b" }
                ] }
            ]
        });
        let request = parse_sampling_request(&params);
        assert_eq!(request.messages[0].content, "a\nb");
    }

    #[test]
    fn build_result_shapes_the_wire_value() {
        let response = SamplingResponse {
            role: "assistant".to_string(),
            content: "answer".to_string(),
            model: Some("test-model".to_string()),
            stop_reason: Some("endTurn".to_string()),
        };
        let value = build_sampling_result(&response);
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "answer");
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["stopReason"], "endTurn");
    }

    struct EchoSampler;
    #[async_trait]
    impl SamplingHandler for EchoSampler {
        async fn create_message(
            &self,
            request: SamplingRequest,
        ) -> Result<SamplingResponse, SamplingError> {
            let last = request
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(SamplingResponse::assistant(format!("echo: {last}")))
        }
    }

    #[tokio::test]
    async fn bridge_answers_sampling_and_rejects_other_methods() {
        let bridge = SamplingBridge::new(Arc::new(EchoSampler));
        let result = bridge
            .handle(
                &json!(1),
                SAMPLING_METHOD,
                json!({ "messages": [{ "role": "user", "content": "ping" }] }),
            )
            .await
            .expect("handled");
        assert_eq!(result["content"]["text"], "echo: ping");

        let err = bridge
            .handle(&json!(2), "roots/list", Value::Null)
            .await
            .expect_err("rejects");
        assert_eq!(err.code, -32601);
    }

    struct FailingSampler;
    #[async_trait]
    impl SamplingHandler for FailingSampler {
        async fn create_message(
            &self,
            _request: SamplingRequest,
        ) -> Result<SamplingResponse, SamplingError> {
            Err(SamplingError("no model bound".to_string()))
        }
    }

    #[tokio::test]
    async fn sampling_failure_maps_to_internal_error() {
        let bridge = SamplingBridge::new(Arc::new(FailingSampler));
        let err = bridge
            .handle(&json!(3), SAMPLING_METHOD, json!({ "messages": [] }))
            .await
            .expect_err("fails");
        assert_eq!(err.code, -32603);
        assert!(err.message.contains("no model bound"));
    }
}
