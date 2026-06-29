//! The neutral LLM port is data-only on the wire and dyn-dispatchable async.

use std::sync::Arc;

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatContent, ChatMessage, ChatRequest, ChatResponse, ChatRole, LlmExecutor,
    ToolCall, ToolSchema,
};
use awaken_runtime_contract::resolved::ModelBinding;

fn sample_request() -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding {
            provider_instance_ref: "provider-1".to_string(),
            model_ref: "gpt-4o-mini".to_string(),
            backend_ref: "backend-1".to_string(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: ChatContent::Text("hello".to_string()),
        }],
        tools: vec![ToolSchema {
            id: "echo".to_string(),
            description: "echo back".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }],
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
        output: AssistantOutput::ToolCalls(vec![ToolCall {
            call_id: "c1".to_string(),
            tool_id: "echo".to_string(),
            arguments: serde_json::json!({"text": "hi"}),
        }]),
        usage: None,
    };
    let json = serde_json::to_string(&response).expect("serialize");
    let back: ChatResponse = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(response, back);
}

/// A deterministic fake provider proves the port is object-safe and awaitable.
struct EchoExecutor;

#[async_trait::async_trait]
impl LlmExecutor for EchoExecutor {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let echoed = match request.messages.last().map(|m| &m.content) {
            Some(ChatContent::Text(text)) => text.clone(),
            _ => String::new(),
        };
        Ok(ChatResponse {
            output: AssistantOutput::Text(echoed),
            usage: None,
        })
    }
}

#[tokio::test]
async fn llm_executor_is_dyn_dispatchable_and_awaitable() {
    let executor: Arc<dyn LlmExecutor> = Arc::new(EchoExecutor);
    let response = executor.infer(sample_request()).await.expect("infer");
    assert_eq!(response.output, AssistantOutput::Text("hello".to_string()));
}
