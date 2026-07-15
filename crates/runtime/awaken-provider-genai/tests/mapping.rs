//! The adapter maps neutral requests onto genai types and routes on the
//! selected model binding without substituting a different model (G22).

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::{
    GenaiExecutor, classify_error, from_genai_tool_call, map_assistant_output, map_usage,
    to_genai_request,
};
use awaken_runtime_contract::llm::{AssistantOutput, ChatMessage, ChatRequest};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use genai::chat::{
    ChatRole as GenaiRole, ContentPart, MessageContent, PromptTokensDetails,
    ToolCall as GenaiToolCall, ToolResponse, Usage,
};

fn binding(model: &str) -> ModelBinding {
    ModelBinding {
        provider_identity_ref: "p".to_string(),
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
                role: Role::System,
                content: vec![ContentBlock::text("be brief")],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("hi")],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::text("ok")],
            },
        ],
        tools: vec![ToolDescriptor::pinned(
            "test",
            "search",
            "search the web",
            serde_json::json!({"type": "object"}),
        )],
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
            role: Role::Tool,
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
            role: Role::User,
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
            role: Role::User,
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
fn retryable_provider_errors_classify_and_are_retryable() {
    for (msg, code) in [
        ("429 Too Many Requests", "rate_limited"),
        ("rate limit exceeded", "rate_limited"),
        ("model is Overloaded", "overloaded"),
        ("503 Service Unavailable", "overloaded"),
        ("request timed out", "timeout"),
        ("connection reset by peer", "provider_error"),
        ("500 Internal Server Error", "provider_error"),
        // An unclassified message defaults to a retryable provider error.
        ("wire fell out of the socket", "provider_error"),
    ] {
        let err = classify_error(msg);
        assert_eq!(err.code(), code, "{msg:?}");
        assert!(err.is_retryable(), "{msg:?} should be retryable");
    }
}

#[test]
fn permanent_provider_errors_classify_and_are_not_retryable() {
    for (msg, code) in [
        ("invalid api key", "unauthorized"),
        ("401 Unauthorized", "unauthorized"),
        ("context length exceeded", "context_overflow"),
        (
            "prompt is too long: 250000 tokens > 200000 maximum",
            "context_overflow",
        ),
        (
            "400 maximum context length is 128000 tokens, please reduce the length",
            "context_overflow",
        ),
        ("404 model not found", "model_not_found"),
        ("400 Bad Request: invalid request body", "invalid_request"),
        ("request blocked by content filter", "content_filtered"),
    ] {
        let err = classify_error(msg);
        assert_eq!(err.code(), code, "{msg:?}");
        assert!(!err.is_retryable(), "{msg:?} should be permanent");
    }
}

#[test]
fn assistant_tool_use_block_maps_to_a_genai_tool_call() {
    // An assistant turn replaying a prior tool request (a ToolUse block) must map
    // to a genai ToolCall part, or a multi-turn tool conversation loses the model's
    // own call and a strict provider rejects the follow-up tool result.
    let request = ChatRequest {
        model_binding: binding("gpt-4o-mini"),
        messages: vec![ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call-7",
                "search",
                serde_json::json!({"q": "rust"}),
            )],
        }],
        tools: Vec::new(),
    };
    let genai = to_genai_request(&request);
    let content = &genai.messages[0].content;
    assert!(
        content.contains_tool_call(),
        "the tool_use maps to a tool call"
    );
    let calls = content.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call-7");
    assert_eq!(calls[0].fn_name, "search");
    assert_eq!(calls[0].fn_arguments, serde_json::json!({"q": "rust"}));
}

#[test]
fn image_url_infers_content_type_from_extension() {
    // A neutral image URL carries no media type; the adapter infers a concrete MIME
    // from the extension (case-insensitively) so the provider gets a usable type,
    // falling back to the generic `image/*` for an unknown or extension-less URL.
    for (url, expected) in [
        ("https://x.test/a.png", "image/png"),
        ("https://x.test/a.PNG", "image/png"),
        ("https://x.test/a.jpg", "image/jpeg"),
        ("https://x.test/a.jpeg", "image/jpeg"),
        ("https://x.test/a.gif", "image/gif"),
        ("https://x.test/a.webp", "image/webp"),
        ("https://x.test/a.bmp", "image/*"),
        ("https://x.test/no-extension", "image/*"),
    ] {
        let request = ChatRequest {
            model_binding: binding("m"),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::image_url(url)],
            }],
            tools: Vec::new(),
        };
        let genai = to_genai_request(&request);
        let binaries = genai.messages[0].content.binaries();
        assert_eq!(binaries.len(), 1, "{url}");
        assert_eq!(binaries[0].content_type, expected, "{url}");
    }
}

#[test]
fn usage_maps_prompt_cache_breakdown_when_present() {
    // Anthropic reports cache_read / cache_creation inside prompt_tokens_details;
    // the adapter surfaces both so cost accounting sees the cache split.
    let usage = Usage {
        prompt_tokens: Some(100),
        completion_tokens: Some(40),
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(70),
            cache_creation_tokens: Some(12),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mapped = map_usage(&usage);
    assert_eq!(mapped.prompt_tokens, 100);
    assert_eq!(mapped.completion_tokens, 40);
    assert_eq!(mapped.cache_read_tokens, 70);
    assert_eq!(mapped.cache_creation_tokens, 12);

    // No details → the cache split is zero, not an error.
    let plain = map_usage(&Usage {
        prompt_tokens: Some(5),
        ..Default::default()
    });
    assert_eq!(plain.cache_read_tokens, 0);
    assert_eq!(plain.cache_creation_tokens, 0);
}

#[test]
fn usage_clamps_negative_counts_to_zero() {
    // A stray negative count (the field is a signed i32) clamps to zero rather
    // than wrapping to a huge u64.
    let usage = Usage {
        prompt_tokens: Some(-5),
        completion_tokens: Some(-1),
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(-3),
            cache_creation_tokens: Some(-2),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mapped = map_usage(&usage);
    assert_eq!(mapped.prompt_tokens, 0);
    assert_eq!(mapped.completion_tokens, 0);
    assert_eq!(mapped.cache_read_tokens, 0);
    assert_eq!(mapped.cache_creation_tokens, 0);
}

#[test]
fn assistant_output_preserves_order_and_drops_non_content_parts() {
    // The doc contract: text and tool requests keep their interleaved order, and
    // provider parts with no neutral equivalent (a tool response) are dropped.
    let content = MessageContent::from_parts(vec![
        ContentPart::Text("first".to_string()),
        ContentPart::ToolCall(GenaiToolCall {
            call_id: "c1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: serde_json::json!({}),
            thought_signatures: None,
        }),
        ContentPart::Text("second".to_string()),
        // A tool response has no neutral assistant-output equivalent → dropped.
        ContentPart::ToolResponse(ToolResponse::new("c1", "ignored")),
    ]);
    let output = map_assistant_output(&content);
    assert_eq!(output.blocks.len(), 3, "the tool response is dropped");
    assert_eq!(output.blocks[0], ContentBlock::text("first"));
    assert!(matches!(output.blocks[1], ContentBlock::ToolUse { .. }));
    assert_eq!(output.blocks[2], ContentBlock::text("second"));
}

#[test]
fn classify_error_numeric_status_and_precedence_boundaries() {
    // Numeric-status rows and cross-class precedence that the phrase-based cases
    // do not cover.
    for (msg, code) in [
        // 413 (payload too large) is deliberately an overflow, not a rate limit.
        ("413 Request Entity Too Large", "context_overflow"),
        ("422 Unprocessable Entity", "invalid_request"),
        ("529 site is overloaded", "overloaded"),
        ("408 Request Timeout", "timeout"),
        ("504 Gateway Timeout", "timeout"),
        // Content-filter is checked first: a 400 that is really a policy block must
        // classify as content_filtered, not the generic invalid_request bucket.
        ("400 request blocked by content policy", "content_filtered"),
        // An empty message has nothing to match → the retryable provider default.
        ("", "provider_error"),
    ] {
        assert_eq!(classify_error(msg).code(), code, "{msg:?}");
    }
}

#[test]
fn genai_stop_reasons_map_onto_neutral_stop_reasons() {
    use awaken_provider_genai::map_stop_reason;
    use awaken_runtime_contract::llm::StopReason;
    use genai::chat::StopReason as GenaiStopReason;

    let cases = [
        (
            GenaiStopReason::Completed("end_turn".to_string()),
            Some(StopReason::EndTurn),
        ),
        (
            GenaiStopReason::MaxTokens("max_tokens".to_string()),
            Some(StopReason::MaxTokens),
        ),
        (
            GenaiStopReason::ToolCall("tool_use".to_string()),
            Some(StopReason::ToolUse),
        ),
        (
            GenaiStopReason::StopSequence("stop_sequence".to_string()),
            Some(StopReason::StopSequence),
        ),
        (
            GenaiStopReason::ContentFilter("SAFETY".to_string()),
            Some(StopReason::ContentFilter),
        ),
        // A provider-specific reason the SDK cannot classify stays unknown.
        (GenaiStopReason::Other("load".to_string()), None),
    ];
    for (genai_reason, expected) in cases {
        assert_eq!(map_stop_reason(&genai_reason), expected);
    }
}
