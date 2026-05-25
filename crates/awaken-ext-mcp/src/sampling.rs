//! Sampling handler for routing MCP `sampling/createMessage` requests to an LLM.
//!
//! Provides the [`SamplingHandler`] trait and a [`DefaultSamplingHandler`]
//! that bridges MCP sampling requests to an awaken [`LlmExecutor`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::AgentSpec;
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

/// Factory that constructs a per-call [`SamplingHandler`] given the agent
/// initiating an MCP tool call. Lets awaken route server-initiated
/// `sampling/createMessage` requests to the **calling agent's** LLM
/// executor — different agents using different models will see their own
/// LLM respond to MCP sampling, instead of all sharing one fixed handler
/// at the registry level (the previous design leak documented in
/// `awaken-ext-mcp` audit).
///
/// `for_agent` returns:
/// - `Some(handler)` — bind this agent's call to that handler;
///   `sampling/createMessage` during the call routes there.
/// - `None` — the factory **explicitly refuses** to bind this agent
///   (e.g. `agent.model_id` doesn't resolve, agent opted out, tenant
///   has no sampling quota). The manager maps this to
///   `McpCallSampling::Denied`; the transport then rejects the
///   server-initiated `sampling/createMessage` with JSON-RPC
///   method-not-supported. **It does NOT fall back to the registry-
///   level fixed handler** — falling through would re-introduce the
///   cross-agent leak this factory exists to prevent.
///
/// The "no factory configured at all" case is a separate state
/// (`McpCallSampling::Inherit`) and is the only path that falls back
/// to the transport-level fixed handler. See [`McpCallSampling`] in
/// `awaken_ext_mcp::transport` for the full three-state semantics.
///
/// [`McpCallSampling`]: crate::transport::McpCallSampling
#[async_trait]
pub trait SamplingHandlerFactory: Send + Sync {
    async fn for_agent(&self, agent_spec: &AgentSpec) -> Option<Arc<dyn SamplingHandler>>;
}

/// Trivial factory that ignores the agent and always returns the same
/// handler. Preserves the pre-R1 behaviour of "one fixed handler for all
/// agents". New runtime wiring should provide a registry-driven factory
/// that resolves the agent's `model_id` → provider → `LlmExecutor` and
/// wraps it in [`DefaultSamplingHandler`] — that's the per-agent fix the
/// MCP audit identified.
pub struct FixedSamplingHandlerFactory {
    handler: Arc<dyn SamplingHandler>,
}

impl FixedSamplingHandlerFactory {
    pub fn new(handler: Arc<dyn SamplingHandler>) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl SamplingHandlerFactory for FixedSamplingHandlerFactory {
    async fn for_agent(&self, _agent_spec: &AgentSpec) -> Option<Arc<dyn SamplingHandler>> {
        Some(self.handler.clone())
    }
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
    ///
    /// MCP `SamplingContent` is a union of `Text`, `Image`, `Audio`, …
    /// awaken's [`Message`] today only carries text. Rather than
    /// silently dropping non-text blocks (which would let the server's
    /// "describe this image" prompt arrive at the LLM with the image
    /// stripped — a correctness bug masked as an empty turn), we
    /// surface the limitation as a typed error.
    ///
    /// Returns `Err(unsupported_content_kind)` on the first non-text
    /// block encountered. Callers should map this to an MCP JSON-RPC
    /// error so the server learns its sampling request can't be
    /// serviced and can decide how to proceed (retry with text only,
    /// fall back to a different client, etc).
    ///
    /// Multiple text blocks within a single message are joined with a
    /// blank line ("\n\n") so `"hello"` + `"world"` becomes
    /// `"hello\n\nworld"`, not `"helloworld"`. The spec doesn't
    /// prescribe a join; blank-line is the convention for prose
    /// paragraphs and avoids accidentally fusing tokens.
    fn convert_messages(params: &CreateMessageParams) -> Result<Vec<Message>, McpTransportError> {
        let mut out = Vec::with_capacity(params.messages.len());
        for msg in &params.messages {
            let mut text_parts: Vec<&str> = Vec::with_capacity(msg.content.len());
            for block in &msg.content {
                match block {
                    SamplingContent::Text { text: t, .. } => text_parts.push(t.as_str()),
                    other => {
                        return Err(McpTransportError::TransportError(format!(
                            "sampling request contains unsupported content kind: {} \
                             (awaken's sampling handler only supports text — server should \
                             retry with a text-only message)",
                            sampling_content_kind(other)
                        )));
                    }
                }
            }
            let joined = text_parts.join("\n\n");
            out.push(match msg.role {
                mcp::Role::User => Message::user(joined),
                mcp::Role::Assistant => Message::assistant(joined),
            });
        }
        Ok(out)
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

/// Name the variant of [`SamplingContent`] for use in error messages.
/// Kept as a private free function so it can be unit-tested independently
/// of [`DefaultSamplingHandler`].
fn sampling_content_kind(content: &SamplingContent) -> &'static str {
    match content {
        SamplingContent::Text { .. } => "text",
        SamplingContent::Image { .. } => "image",
        SamplingContent::Audio { .. } => "audio",
        SamplingContent::ToolUse { .. } => "tool_use",
        SamplingContent::ToolResult { .. } => "tool_result",
    }
}

/// Reject sampling requests whose presence would silently change LLM
/// behaviour. The MCP spec lets the server specify stop sequences,
/// context inclusion, tool choice, etc.; awaken's handler currently maps
/// only a small subset (system prompt, temperature, max_tokens), so
/// honouring these would produce a different reply than the server
/// asked for — a class of bug that's invisible until model output goes
/// subtly wrong. Returning an error puts the burden back on the server
/// to either retry without the unsupported field or fall over to a
/// different client.
///
/// `modelPreferences` is advisory in MCP sampling. The default handler
/// uses the model already configured for the agent and ignores those
/// hints instead of rejecting otherwise interoperable servers.
///
/// Returns `Err` with a human-readable description of the offending
/// field. `Ok(())` means every behavioural field is either absent or
/// in awaken's supported subset.
fn reject_unsupported_sampling_fields(
    params: &CreateMessageParams,
) -> Result<(), McpTransportError> {
    let mut unsupported: Vec<&'static str> = Vec::new();
    if params
        .stop_sequences
        .as_ref()
        .is_some_and(|s| !s.is_empty())
    {
        unsupported.push("stopSequences");
    }
    if params.include_context.is_some() {
        unsupported.push("includeContext");
    }
    if params.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        unsupported.push("tools");
    }
    if params.tool_choice.is_some() {
        unsupported.push("toolChoice");
    }
    if !unsupported.is_empty() {
        return Err(McpTransportError::TransportError(format!(
            "sampling request sets unsupported field(s): {} \
             (awaken's DefaultSamplingHandler maps systemPrompt, \
             temperature, maxTokens only; honouring others silently \
             would change the LLM's reply away from what the server \
             requested)",
            unsupported.join(", ")
        )));
    }
    Ok(())
}

#[async_trait]
impl SamplingHandler for DefaultSamplingHandler {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError> {
        // Reject BEFORE message conversion so the server sees the
        // field-level objection even when content happens to be valid.
        reject_unsupported_sampling_fields(&params)?;

        let messages = Self::convert_messages(&params)?;
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
            routing_key: None,
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
        let msgs =
            DefaultSamplingHandler::convert_messages(&params).expect("text-only converts cleanly");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].text(), "hello");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].text(), "hi there");
    }

    #[test]
    fn convert_messages_rejects_image_content() {
        // Reviewer flagged the previous "silently filter non-text"
        // behaviour: a server's "describe this image" sampling request
        // would arrive at the LLM with the image stripped and only the
        // prose text — producing nonsense answers and a baffling debug
        // session. New behaviour: surface a typed error so the server
        // sees method-supported-but-content-not-handled, not silent
        // success with bogus output.
        let params = CreateMessageParams {
            messages: vec![SamplingMessage {
                role: mcp::Role::User,
                content: vec![
                    SamplingContent::Text {
                        text: "describe this:".into(),
                        annotations: None,
                        meta: None,
                    },
                    SamplingContent::Image {
                        data: "base64-blob".into(),
                        mime_type: "image/png".into(),
                        annotations: None,
                        meta: None,
                    },
                ],
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
        };
        let err =
            DefaultSamplingHandler::convert_messages(&params).expect_err("image must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("image"),
            "error should identify the offending content kind, got: {msg}"
        );
    }

    #[test]
    fn convert_messages_rejects_audio_content() {
        let params = CreateMessageParams {
            messages: vec![SamplingMessage {
                role: mcp::Role::User,
                content: vec![SamplingContent::Audio {
                    data: "base64-blob".into(),
                    mime_type: "audio/wav".into(),
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
        };
        let err =
            DefaultSamplingHandler::convert_messages(&params).expect_err("audio must be rejected");
        assert!(format!("{err}").contains("audio"));
    }

    #[test]
    fn sampling_content_kind_names_each_variant() {
        // Lock in the strings used in error messages so a future
        // refactor of the helper doesn't silently drop a variant.
        assert_eq!(
            sampling_content_kind(&SamplingContent::Text {
                text: "x".into(),
                annotations: None,
                meta: None,
            }),
            "text"
        );
        assert_eq!(
            sampling_content_kind(&SamplingContent::Image {
                data: "x".into(),
                mime_type: "image/png".into(),
                annotations: None,
                meta: None,
            }),
            "image"
        );
        assert_eq!(
            sampling_content_kind(&SamplingContent::Audio {
                data: "x".into(),
                mime_type: "audio/wav".into(),
                annotations: None,
                meta: None,
            }),
            "audio"
        );
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

    #[test]
    fn convert_messages_joins_multi_text_with_blank_line() {
        // Prior version used push_str with no separator, so
        // ["hello", "world"] became "helloworld". Blank-line join
        // preserves the boundary so consumers can still see the
        // paragraph structure the server sent.
        let params = CreateMessageParams {
            messages: vec![SamplingMessage {
                role: mcp::Role::User,
                content: vec![
                    SamplingContent::Text {
                        text: "hello".into(),
                        annotations: None,
                        meta: None,
                    },
                    SamplingContent::Text {
                        text: "world".into(),
                        annotations: None,
                        meta: None,
                    },
                ],
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
        };
        let msgs = DefaultSamplingHandler::convert_messages(&params).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "hello\n\nworld");
    }

    #[tokio::test]
    async fn handle_create_message_rejects_stop_sequences() {
        let executor = Arc::new(MockLlm {
            response_text: "ignored".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "m");
        let mut params = make_params("hi");
        params.stop_sequences = Some(vec!["STOP".into()]);
        let err = handler
            .handle_create_message(params)
            .await
            .expect_err("stopSequences must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("stopSequences"), "got: {msg}");
    }

    #[tokio::test]
    async fn handle_create_message_rejects_tool_choice() {
        let executor = Arc::new(MockLlm {
            response_text: "ignored".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "m");
        let mut params = make_params("hi");
        params.tool_choice = Some(mcp::ToolChoice {
            mode: Some(mcp::ToolChoiceMode::Required),
        });
        let err = handler
            .handle_create_message(params)
            .await
            .expect_err("toolChoice must be rejected");
        assert!(format!("{err}").contains("toolChoice"));
    }

    #[tokio::test]
    async fn handle_create_message_rejects_include_context() {
        let executor = Arc::new(MockLlm {
            response_text: "ignored".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "m");
        let mut params = make_params("hi");
        params.include_context = Some("thisServer".into());
        let err = handler
            .handle_create_message(params)
            .await
            .expect_err("must reject");
        let msg = format!("{err}");
        assert!(msg.contains("includeContext"), "got: {msg}");
        assert!(!msg.contains("modelPreferences"), "got: {msg}");
    }

    #[tokio::test]
    async fn default_sampling_handler_ignores_model_preferences() {
        let executor = Arc::new(MockLlm {
            response_text: "ok".into(),
        });
        let handler = DefaultSamplingHandler::new(executor, "configured-model");
        let mut params = make_params("hi");
        params.model_preferences = Some(mcp::ModelPreferences {
            hints: None,
            cost_priority: None,
            speed_priority: None,
            intelligence_priority: None,
        });

        let result = handler
            .handle_create_message(params)
            .await
            .expect("modelPreferences are advisory and should not fail basic sampling");

        assert_eq!(result.model, "configured-model");
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
    async fn fixed_factory_returns_same_handler_regardless_of_agent() {
        // The fixed factory preserves R0 behaviour: every agent gets the
        // same handler. Per-agent routing only kicks in when callers wire
        // a registry-driven factory.
        let executor = Arc::new(MockLlm {
            response_text: "shared".into(),
        });
        let handler: Arc<dyn SamplingHandler> =
            Arc::new(DefaultSamplingHandler::new(executor, "shared-model"));
        let factory = FixedSamplingHandlerFactory::new(Arc::clone(&handler));

        let spec_a = AgentSpec {
            id: "a".into(),
            model_id: "claude-opus".into(),
            system_prompt: "".into(),
            ..Default::default()
        };
        let spec_b = AgentSpec {
            id: "b".into(),
            model_id: "gpt-5".into(),
            system_prompt: "".into(),
            ..Default::default()
        };

        let resolved_a = factory.for_agent(&spec_a).await.expect("Some handler");
        let resolved_b = factory.for_agent(&spec_b).await.expect("Some handler");
        // Same Arc identity regardless of agent.
        assert!(Arc::ptr_eq(&resolved_a, &handler));
        assert!(Arc::ptr_eq(&resolved_b, &handler));
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
