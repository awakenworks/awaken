//! The neutral LLM port is data-only on the wire and dyn-dispatchable async.

use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatMessage, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};

fn sample_request() -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "provider-1".to_string(),
            model_ref: "gpt-4o-mini".to_string(),
            backend_ref: "backend-1".to_string(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("hello")],
        }],
        tools: vec![ToolDescriptor::pinned(
            "test",
            "echo",
            "echo back",
            serde_json::json!({"type": "object"}),
        )],
    }
}

#[test]
fn chat_request_round_trips_as_plain_data() {
    let request = sample_request();
    let json = serde_json::to_string(&request).expect("serialize");
    let back: ChatRequest = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(request, back);
}

#[test]
fn chat_response_round_trips_with_tool_calls() {
    let response = ChatResponse {
        output: AssistantOutput::from_tool_calls(vec![ToolCall {
            call_id: "c1".to_string(),
            tool_id: "echo".to_string(),
            arguments: serde_json::json!({"text": "hi"}),
        }]),
        usage: None,
        stop_reason: None,
    };
    let json = serde_json::to_string(&response).expect("serialize");
    let back: ChatResponse = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(response, back);
}

#[test]
fn chat_response_stop_reason_round_trips_and_defaults_when_absent() {
    use awaken_runtime_contract::llm::StopReason;

    let response = ChatResponse {
        output: AssistantOutput::text("cut off"),
        usage: None,
        stop_reason: Some(StopReason::MaxTokens),
    };
    let json = serde_json::to_string(&response).expect("serialize");
    let back: ChatResponse = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(response, back);

    // A response recorded before the field existed still deserializes: the
    // absent stop reason defaults to `None` (unknown).
    let legacy = r#"{"output":{"blocks":[{"type":"text","text":"hi"}]},"usage":null}"#;
    let back: ChatResponse = serde_json::from_str(legacy).expect("legacy deserializes");
    assert_eq!(back.stop_reason, None);
}

/// A deterministic fake provider proves the port is object-safe and awaitable.
struct EchoExecutor;

#[async_trait::async_trait]
impl LlmExecutor for EchoExecutor {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let echoed = request
            .messages
            .last()
            .map(|m| extract_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(echoed),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn llm_executor_is_dyn_dispatchable_and_awaitable() {
    let executor: Arc<dyn LlmExecutor> = Arc::new(EchoExecutor);
    let response = executor.infer(sample_request()).await.expect("infer");
    assert_eq!(response.output, AssistantOutput::text("hello".to_string()));
}
