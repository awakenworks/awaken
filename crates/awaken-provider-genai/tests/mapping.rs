//! The adapter maps neutral requests onto genai types and routes on the
//! selected model binding without substituting a different model (G22).

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_provider_genai::{
    GenaiExecutor, classify_error, from_genai_tool_call, map_assistant_output, map_usage,
    to_genai_request,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatMessage, ChatRequest, ChatRole, Error as LlmError, ToolSchema,
};
use awaken_runtime_contract::resolved::ModelBinding;
use genai::chat::{ChatRole as GenaiRole, MessageContent, ToolCall as GenaiToolCall, Usage};

fn binding(model: &str) -> ModelBinding {
    ModelBinding {
        provider_instance_ref: "p".to_string(),
        model_ref: model.to_string(),
        backend_ref: "b".to_string(),
    }
}

#[test]
fn maps_roles_and_tools_onto_genai_request() {
    let request = ChatRequest {
        model_binding: binding("gpt-4o-mini"),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ContentBlock::text("be brief")],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::text("hi")],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ContentBlock::text("ok")],
            },
        ],
        tools: vec![ToolSchema {
            id: "search".to_string(),
            description: "search the web".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }],
    };

    let genai = to_genai_request(&request);
    assert_eq!(genai.messages.len(), 3);
    assert!(matches!(genai.messages[0].role, GenaiRole::System));
    assert!(matches!(genai.messages[1].role, GenaiRole::User));
    assert!(matches!(genai.messages[2].role, GenaiRole::Assistant));
    assert_eq!(genai.tools.as_ref().map(|t| t.len()), Some(1));
}

#[test]
fn tool_result_maps_to_a_genai_tool_message() {
    // A tool result must become a genai *tool* message (not a user message), or a
    // strict provider rejects the multi-turn tool conversation with
    // "tool_call_ids did not have response messages". Regression for that bug.
    let request = ChatRequest {
        model_binding: binding("gpt-4o-mini"),
        messages: vec![ChatMessage {
            role: ChatRole::Tool,
            content: vec![ContentBlock::tool_result(
                "call-1",
                vec![ContentBlock::text("file body")],
            )],
        }],
        tools: Vec::new(),
    };

    let genai = to_genai_request(&request);
    assert_eq!(genai.messages.len(), 1);
    assert!(matches!(genai.messages[0].role, GenaiRole::Tool));
}

#[test]
fn image_block_maps_to_a_binary_part() {
    let request = ChatRequest {
        model_binding: binding("gpt-4o-mini"),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![
                ContentBlock::text("what is this?"),
                ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
            ],
        }],
        tools: Vec::new(),
    };

    let genai = to_genai_request(&request);
    let content = &genai.messages[0].content;
    assert!(content.contains_text(), "the text block survives");
    assert!(content.contains_binary(), "the image maps to a binary part");
    let binaries = content.binaries();
    assert_eq!(binaries.len(), 1);
    assert_eq!(binaries[0].content_type, "image/png");
}

#[test]
fn omits_tools_when_none_are_visible() {
    let request = ChatRequest {
        model_binding: binding("m"),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("hi")],
        }],
        tools: Vec::new(),
    };
    assert!(to_genai_request(&request).tools.is_none());
}

#[test]
fn maps_genai_tool_call_back_to_neutral() {
    let genai = GenaiToolCall {
        call_id: "c9".to_string(),
        fn_name: "calc".to_string(),
        fn_arguments: serde_json::json!({"expr": "2+2"}),
        thought_signatures: None,
    };
    let neutral = from_genai_tool_call(&genai);
    assert_eq!(neutral.call_id, "c9");
    assert_eq!(neutral.tool_id, "calc");
    assert_eq!(neutral.arguments, serde_json::json!({"expr": "2+2"}));
}

#[test]
fn text_content_maps_to_text_output() {
    let content = MessageContent::from_text("hello world");
    assert_eq!(
        map_assistant_output(&content),
        AssistantOutput::text("hello world".to_string())
    );
}

#[test]
fn tool_call_content_maps_to_tool_calls_output() {
    let content = MessageContent::from_tool_calls(vec![GenaiToolCall {
        call_id: "c1".to_string(),
        fn_name: "search".to_string(),
        fn_arguments: serde_json::json!({"q": "rust"}),
        thought_signatures: None,
    }]);
    let calls = map_assistant_output(&content).tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_id, "search");
}

#[test]
fn usage_maps_and_clamps_missing_or_negative_to_zero() {
    let usage = Usage {
        prompt_tokens: Some(10),
        completion_tokens: Some(20),
        ..Default::default()
    };
    let mapped = map_usage(&usage);
    assert_eq!(mapped.prompt_tokens, 10);
    assert_eq!(mapped.completion_tokens, 20);

    // Absent counts clamp to zero rather than panicking.
    let zero = map_usage(&Usage::default());
    assert_eq!(zero.prompt_tokens, 0);
    assert_eq!(zero.completion_tokens, 0);
}

#[test]
fn executor_constructors_are_available() {
    let _ = GenaiExecutor::new();
    let _ = GenaiExecutor::default().with_timeout(Duration::from_secs(5));
    let _ = GenaiExecutor::with_client(genai::Client::default());
}

#[test]
fn transient_provider_errors_are_retryable() {
    for msg in [
        "429 Too Many Requests",
        "model is Overloaded",
        "rate limit exceeded",
        "503 Service Unavailable",
        "connection reset by peer",
        "request timed out",
    ] {
        assert!(
            matches!(classify_error(msg), LlmError::Transient(_)),
            "{msg:?} should be transient"
        );
    }
}

#[test]
fn permanent_provider_errors_are_not_retryable() {
    for msg in [
        "invalid api key",
        "401 Unauthorized",
        "context length exceeded",
        "model not found",
    ] {
        assert!(
            matches!(classify_error(msg), LlmError::Inference(_)),
            "{msg:?} should be permanent"
        );
    }
}
