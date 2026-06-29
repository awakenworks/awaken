//! `genai`-backed implementation of the runtime's `LlmExecutor` port.
//!
//! This adapter is the only crate allowed to name the model SDK. It maps the
//! neutral `ChatRequest`/`ChatResponse` onto `genai` types and routes on the
//! selected `ModelBinding.model_ref` — it never picks a different model (G22).

use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, extract_text};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, Error, LlmExecutor, Result, TokenUsage,
    ToolCall,
};
use genai::Client;
use genai::chat::{
    Binary, ChatMessage, ChatRequest as GenaiChatRequest, ContentPart, MessageContent,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse, Usage,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// A `genai::Client` behind the neutral `LlmExecutor` port.
pub struct GenaiExecutor {
    client: Client,
    timeout: Duration,
}

impl Default for GenaiExecutor {
    fn default() -> Self {
        Self {
            client: Client::default(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl GenaiExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a pre-configured client (custom auth, endpoints, adapters).
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl LlmExecutor for GenaiExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request(&request);

        let response = tokio::time::timeout(
            self.timeout,
            self.client.exec_chat(model, genai_request, None),
        )
        .await
        // A timeout is transient: the same call may succeed on retry.
        .map_err(|_| Error::Transient("model call timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        Ok(from_genai_response(response))
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> Result<ChatResponse> {
        use futures::StreamExt;
        use genai::chat::{ChatOptions, ChatStreamEvent};

        let model = request.model_binding.model_ref.clone();
        let genai_request = to_genai_request(&request);

        // Have genai assemble the committed turn for us. It concatenates the text
        // chunks and parses the accumulated tool-argument fragments into a JSON
        // object, exactly as the non-streaming path returns them — Anthropic
        // streams tool arguments as `input_json_delta` text that is only valid
        // JSON once the block ends. Without `capture_*` the `End` event carries
        // no content and we'd be stuck with the raw, string-encoded deltas.
        let options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true);

        let stream_response = tokio::time::timeout(
            self.timeout,
            self.client
                .exec_chat_stream(model, genai_request, Some(&options)),
        )
        .await
        .map_err(|_| Error::Transient("model stream timed out".to_string()))?
        .map_err(|err| classify_error(&err.to_string()))?;

        let mut stream = stream_response.stream;
        let mut captured: Option<MessageContent> = None;
        let mut usage: Option<TokenUsage> = None;
        // Fallback assembly, used only if the provider delivers no captured turn.
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(event) = stream.next().await {
            match event.map_err(|err| classify_error(&err.to_string()))? {
                // Live text: forward each chunk as it arrives (valid UTF-8 by type).
                ChatStreamEvent::Chunk(chunk) if !chunk.content.is_empty() => {
                    sink.on_text(&chunk.content).await;
                    text.push_str(&chunk.content);
                }
                // Live tool-call progress: genai hands incremental, string-encoded
                // arguments here. Best-effort only; the committed turn is the End
                // event, where genai has parsed the arguments into an object.
                ChatStreamEvent::ToolCallChunk(tool) => {
                    let call = from_genai_tool_call(&tool.tool_call);
                    sink.on_tool_call(&call.call_id, &call.tool_id, &call.arguments)
                        .await;
                    match tool_calls.iter_mut().find(|c| c.call_id == call.call_id) {
                        Some(existing) => *existing = call,
                        None => tool_calls.push(call),
                    }
                }
                // Committed turn: genai's parsed, ordered content and usage.
                ChatStreamEvent::End(end) => {
                    captured = end.captured_content;
                    usage = end.captured_usage.as_ref().map(map_usage);
                }
                _ => {}
            }
        }

        // Prefer genai's captured turn (parsed tool arguments, same shape as
        // `infer`); fall back to the chunk-assembled blocks only if no End
        // content arrived (e.g. a stream that ends without a captured block).
        let output = match captured {
            Some(content) => map_assistant_output(&content),
            None => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text));
                }
                for call in tool_calls {
                    blocks.push(ContentBlock::tool_use(
                        call.call_id,
                        call.tool_id,
                        call.arguments,
                    ));
                }
                AssistantOutput::from_blocks(blocks)
            }
        };
        Ok(ChatResponse { output, usage })
    }
}

/// Classify a provider error string as transient (retryable) or permanent.
/// Rate limits, overloads, 5xx, and connection resets are worth retrying; auth,
/// quota, and bad-request style failures are not.
pub fn classify_error(message: &str) -> Error {
    let lower = message.to_lowercase();
    const TRANSIENT: &[&str] = &[
        "rate limit",
        "ratelimit",
        "overloaded",
        "too many requests",
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection reset",
        "connection closed",
        "temporarily",
        "unavailable",
    ];
    if TRANSIENT.iter().any(|needle| lower.contains(needle)) {
        Error::Transient(message.to_string())
    } else {
        Error::Inference(message.to_string())
    }
}

/// Map the neutral request onto a `genai::ChatRequest`.
pub fn to_genai_request(request: &ChatRequest) -> GenaiChatRequest {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let parts: Vec<ContentPart> = message.content.iter().map(to_genai_part).collect();
        let genai_message = match message.role {
            ChatRole::System => ChatMessage::system(parts),
            ChatRole::Assistant => ChatMessage::assistant(parts),
            // A tool-role message without structured tool framing is plain input.
            ChatRole::User | ChatRole::Tool => ChatMessage::user(parts),
        };
        messages.push(genai_message);
    }

    let tools: Vec<GenaiTool> = request
        .tools
        .iter()
        .map(|tool| {
            GenaiTool::new(tool.id.clone())
                .with_description(tool.description.clone())
                .with_schema(tool.parameters.clone())
        })
        .collect();

    let mut genai_request = GenaiChatRequest::new(messages);
    if !tools.is_empty() {
        genai_request = genai_request.with_tools(tools);
    }
    genai_request
}

/// Map one neutral content block onto a `genai` content part. Text maps to text;
/// an image maps to a `Binary` (base64 inline or a URL the provider fetches).
fn to_genai_part(block: &ContentBlock) -> ContentPart {
    match block {
        ContentBlock::Text { text } => ContentPart::Text(text.clone()),
        ContentBlock::Image { source } => ContentPart::Binary(to_genai_binary(source)),
        ContentBlock::ToolUse { id, name, input } => ContentPart::ToolCall(GenaiToolCall {
            call_id: id.clone(),
            fn_name: name.clone(),
            fn_arguments: input.clone(),
            thought_signatures: None,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
        } => ContentPart::ToolResponse(ToolResponse::new(
            tool_use_id.clone(),
            extract_text(content),
        )),
    }
}

fn to_genai_binary(source: &ImageSource) -> Binary {
    match source {
        ImageSource::Base64 { media_type, data } => {
            Binary::from_base64(media_type.clone(), data.clone(), None)
        }
        ImageSource::Url { url } => Binary::from_url(content_type_for_url(url), url.clone(), None),
    }
}

/// A neutral image URL carries no media type; infer one from the extension so
/// the provider gets a concrete MIME type, defaulting to a generic image type.
fn content_type_for_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/*"
    }
    .to_string()
}

/// Map a `genai::ToolCall` back into the neutral `ToolCall`.
pub fn from_genai_tool_call(call: &GenaiToolCall) -> ToolCall {
    ToolCall {
        call_id: call.call_id.clone(),
        tool_id: call.fn_name.clone(),
        arguments: call.fn_arguments.clone(),
    }
}

/// Map a `genai::ChatResponse` onto the neutral `ChatResponse`.
pub fn from_genai_response(response: genai::chat::ChatResponse) -> ChatResponse {
    ChatResponse {
        output: map_assistant_output(&response.content),
        usage: Some(map_usage(&response.usage)),
    }
}

/// Map a provider turn onto neutral content blocks, preserving the order of text
/// and tool requests so an interleaved turn (text + tool call + text) survives.
pub fn map_assistant_output(content: &MessageContent) -> AssistantOutput {
    let blocks = content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(ContentBlock::text(text.clone())),
            ContentPart::ToolCall(call) => Some(ContentBlock::tool_use(
                call.call_id.clone(),
                call.fn_name.clone(),
                call.fn_arguments.clone(),
            )),
            _ => None,
        })
        .collect();
    AssistantOutput::from_blocks(blocks)
}

pub fn map_usage(usage: &Usage) -> TokenUsage {
    TokenUsage {
        prompt_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
        completion_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
    }
}
