//! `genai`-backed implementation of the runtime's `LlmExecutor` port.
//!
//! This adapter is the only crate allowed to name the model SDK. It maps the
//! neutral `ChatRequest`/`ChatResponse` onto `genai` types and routes on the
//! selected `ModelBinding.model_ref` — it never picks a different model (G22).

use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatContent, ChatRequest, ChatResponse, ChatRole, Error, LlmExecutor, Result,
    TokenUsage, ToolCall,
};
use genai::Client;
use genai::chat::{
    ChatMessage, ChatRequest as GenaiChatRequest, MessageContent, Tool as GenaiTool,
    ToolCall as GenaiToolCall, ToolResponse, Usage,
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
        match &message.content {
            ChatContent::Text(text) => match message.role {
                ChatRole::System => messages.push(ChatMessage::system(text.clone())),
                ChatRole::Assistant => messages.push(ChatMessage::assistant(text.clone())),
                // A bare tool-role text without a call id is treated as user input.
                ChatRole::User | ChatRole::Tool => messages.push(ChatMessage::user(text.clone())),
            },
            ChatContent::ToolCalls(calls) => {
                let genai_calls: Vec<GenaiToolCall> =
                    calls.iter().map(to_genai_tool_call).collect();
                messages.push(ChatMessage::from(genai_calls));
            }
            ChatContent::ToolResult { call_id, content } => {
                messages.push(ChatMessage::from(ToolResponse::new(
                    call_id.clone(),
                    content.clone(),
                )));
            }
        }
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

fn to_genai_tool_call(call: &ToolCall) -> GenaiToolCall {
    GenaiToolCall {
        call_id: call.call_id.clone(),
        fn_name: call.tool_id.clone(),
        fn_arguments: call.arguments.clone(),
        thought_signatures: None,
    }
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

/// Tool calls take precedence over text, matching the loop's natural-end vs
/// tool-call branch.
pub fn map_assistant_output(content: &MessageContent) -> AssistantOutput {
    let tool_calls = content.tool_calls();
    if tool_calls.is_empty() {
        AssistantOutput::Text(content.first_text().unwrap_or_default().to_string())
    } else {
        AssistantOutput::ToolCalls(tool_calls.into_iter().map(from_genai_tool_call).collect())
    }
}

pub fn map_usage(usage: &Usage) -> TokenUsage {
    TokenUsage {
        prompt_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
        completion_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
    }
}
