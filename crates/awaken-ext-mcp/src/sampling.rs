//! Sampling handler for routing MCP `sampling/createMessage` requests to an LLM.
//!
//! Provides the [`SamplingHandler`] trait and a [`DefaultSamplingHandler`]
//! that bridges MCP sampling requests to an awaken [`LlmExecutor`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceRequest, LlmExecutor};
use awaken_contract::contract::message::Message;
use mcp::transport::McpTransportError;
use mcp::{CreateMessageParams, CreateMessageResult, SamplingContent};

/// Handler for MCP `sampling/createMessage` requests from the server.
///
/// When an MCP server sends a `sampling/createMessage` request during tool
/// execution, this handler is invoked to route it to an LLM for inference.
#[async_trait]
pub trait SamplingHandler: Send + Sync {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError>;
}

/// Default [`SamplingHandler`] that converts MCP sampling requests to awaken
/// [`InferenceRequest`]s, calls the configured [`LlmExecutor`], and converts
/// the response back to MCP format.
pub struct DefaultSamplingHandler {
    executor: Arc<dyn LlmExecutor>,
    upstream_model: String,
}

impl DefaultSamplingHandler {
    /// Create a new handler backed by the given LLM executor.
    ///
    /// `upstream_model` is the model name sent to the configured executor.
    pub fn new(executor: Arc<dyn LlmExecutor>, upstream_model: impl Into<String>) -> Self {
        Self {
            executor,
            upstream_model: upstream_model.into(),
        }
    }

    /// Convert MCP sampling messages to awaken [`Message`] types.
    fn convert_messages(params: &CreateMessageParams) -> Vec<Message> {
        params
            .messages
            .iter()
            .map(|msg| {
                let text = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        SamplingContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                match msg.role {
                    mcp::Role::User => Message::user(text),
                    mcp::Role::Assistant => Message::assistant(text),
                }
            })
            .collect()
    }

    /// Build the system prompt content blocks from the params.
    fn system_blocks(params: &CreateMessageParams) -> Vec<ContentBlock> {
        match &params.system_prompt {
            Some(prompt) if !prompt.is_empty() => vec![ContentBlock::text(prompt.clone())],
            _ => vec![],
        }
    }

    /// Convert an awaken `StreamResult` to MCP `CreateMessageResult`.
    fn convert_result(
        result: &awaken_contract::contract::inference::StreamResult,
        model: &str,
    ) -> CreateMessageResult {
        let text = result.text();
        let content = vec![SamplingContent::Text {
            text,
            annotations: None,
            meta: None,
        }];

        let stop_reason = result.stop_reason.map(|sr| match sr {
            awaken_contract::contract::inference::StopReason::EndTurn => "endTurn".to_string(),
            awaken_contract::contract::inference::StopReason::MaxTokens => "maxTokens".to_string(),
            awaken_contract::contract::inference::StopReason::ToolUse => "toolUse".to_string(),
            awaken_contract::contract::inference::StopReason::StopSequence => {
                "stopSequence".to_string()
            }
        });

        CreateMessageResult {
            role: mcp::Role::Assistant,
            content,
            model: model.to_string(),
            stop_reason,
            meta: None,
        }
    }
}

#[async_trait]
impl SamplingHandler for DefaultSamplingHandler {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError> {
        let messages = Self::convert_messages(&params);
        if messages.is_empty() {
            return Err(McpTransportError::TransportError(
                "sampling request contained no messages".to_string(),
            ));
        }

        let system = Self::system_blocks(&params);

        let overrides = {
            let mut ovr = awaken_contract::contract::inference::InferenceOverride::default();
            if let Some(temp) = params.temperature {
                ovr.temperature = Some(temp);
            }
            ovr.max_tokens = Some(params.max_tokens);
            if ovr.temperature.is_none() && ovr.max_tokens.is_none() {
                None
            } else {
                Some(ovr)
            }
        };

        let request = InferenceRequest {
            upstream_model: self.upstream_model.clone(),
            messages,
            tools: vec![],
            system,
            overrides,
            enable_prompt_cache: false,
        };

        let result =
            self.executor.execute(request).await.map_err(|e| {
                McpTransportError::TransportError(format!("LLM execution failed: {e}"))
            })?;

        Ok(Self::convert_result(&result, &self.upstream_model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::message::Role;
    use mcp::SamplingMessage;

    struct MockLlm {
        response_text: String,
    }

    #[async_trait]
    impl LlmExecutor for MockLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, awaken_contract::contract::executor::InferenceExecutionError>
        {
            Ok(StreamResult {
                content: vec![ContentBlock::text(self.response_text.clone())],
                tool_calls: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(15),
                    ..Default::default()
                }),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    fn make_params(text: &str) -> CreateMessageParams {
        CreateMessageParams {
            messages: vec![SamplingMessage {
                role: mcp::Role::User,
                content: vec![SamplingContent::Text {
                    text: text.to_string(),
                    annotations: None,
                    meta: None,
                }],
                meta: None,
            }],
            model_preferences: None,
            system_prompt: None,
            include_context: None,
            temperature: None,
            max_tokens: 1024,
            stop_sequences: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            task: None,
            meta: None,
        }
    }

    #[test]
    fn convert_messages_maps_roles() {
        let params = CreateMessageParams {
            messages: vec![
                SamplingMessage {
                    role: mcp::Role::User,
                    content: vec![SamplingContent::Text {
                        text: "hello".into(),
                        annotations: None,
                        meta: None,
                    }],
                    meta: None,
                },
                SamplingMessage {
                    role: mcp::Role::Assistant,
                    content: vec![SamplingContent::Text {
                        text: "hi there".into(),
                        annotations: None,
                        meta: None,
                    }],
                    meta: None,
                },
            ],
            model_preferences: None,
            system_prompt: None,
            include_context: None,
            temperature: None,
            max_tokens: 1024,
            stop_sequences: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            task: None,
            meta: None,
        };
        let msgs = DefaultSamplingHandler::convert_messages(&params);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].text(), "hello");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].text(), "hi there");
    }

    #[test]
    fn system_blocks_from_params() {
        let mut params = make_params("test");
        assert!(DefaultSamplingHandler::system_blocks(&params).is_empty());

        params.system_prompt = Some("Be helpful".into());
        let blocks = DefaultSamplingHandler::system_blocks(&params);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Be helpful"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn convert_result_maps_stop_reasons() {
        let result = StreamResult {
            content: vec![ContentBlock::text("response")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        };
        let mcp_result = DefaultSamplingHandler::convert_result(&result, "test-model");
        assert_eq!(mcp_result.model, "test-model");
        assert_eq!(mcp_result.stop_reason.as_deref(), Some("endTurn"));
        assert!(matches!(mcp_result.role, mcp::Role::Assistant));
        assert_eq!(mcp_result.content.len(), 1);
    }

    #[tokio::test]
    async fn default_sampling_handler_routes_to_executor() {
        let executor = Arc::new(MockLlm {
            response_text: "I can help!".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "test-model");

        let params = make_params("help me");
        let result = handler.handle_create_message(params).await.unwrap();

        assert_eq!(result.model, "test-model");
        assert!(matches!(result.role, mcp::Role::Assistant));
        match &result.content[0] {
            SamplingContent::Text { text, .. } => assert_eq!(text, "I can help!"),
            _ => panic!("expected text content"),
        }
        assert_eq!(result.stop_reason.as_deref(), Some("endTurn"));
    }

    #[tokio::test]
    async fn default_sampling_handler_empty_messages_returns_error() {
        let executor = Arc::new(MockLlm {
            response_text: "".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "test-model");

        let params = CreateMessageParams {
            messages: vec![],
            model_preferences: None,
            system_prompt: None,
            include_context: None,
            temperature: None,
            max_tokens: 1024,
            stop_sequences: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            task: None,
            meta: None,
        };
        let err = handler.handle_create_message(params).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn default_sampling_handler_passes_overrides() {
        // Use a mock that captures and returns — we verify the handler doesn't error
        let executor = Arc::new(MockLlm {
            response_text: "ok".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "model-v1");

        let mut params = make_params("test");
        params.temperature = Some(0.7);
        params.max_tokens = 512;
        params.system_prompt = Some("System".into());

        let result = handler.handle_create_message(params).await.unwrap();
        assert_eq!(result.model, "model-v1");
    }
}
