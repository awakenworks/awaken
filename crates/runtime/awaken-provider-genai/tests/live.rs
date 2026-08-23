//! Live provider smoke tests. Ignored by default; they need network and an
//! explicit provider API key in the environment. These tests read it themselves
//! and inject it into fixed adapters; provider adapters have no ambient default.
//! Run with:
//!
//! ```sh
//! AWAKEN_GENAI_MODEL=gpt-4o-mini cargo test -p awaken-provider-genai --test live -- --ignored
//! ```

mod support;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::{GenaiExecutor, OpenAiResponsesExecutor};
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, DeltaSink, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use std::sync::Mutex;
use support::compatibility_tool;

#[derive(Default)]
struct RecordingDeltas {
    text: Mutex<String>,
    reasoning: Mutex<String>,
    tool_arguments: Mutex<String>,
}

#[async_trait::async_trait]
impl DeltaSink for RecordingDeltas {
    async fn on_text(&self, chunk: &str) {
        self.text.lock().unwrap().push_str(chunk);
    }

    async fn on_reasoning(&self, chunk: &str) {
        self.reasoning.lock().unwrap().push_str(chunk);
    }

    async fn on_tool_call_delta(&self, _call_id: &str, _tool_id: &str, args_delta: &str) {
        self.tool_arguments.lock().unwrap().push_str(args_delta);
    }
}

fn live_executor() -> (GenaiExecutor, String) {
    let deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
    let (adapter, base_url, key, default_model) = if let Some(key) = deepseek {
        (
            awaken_provider_genai::AdapterKind::OpenAI,
            Some("https://api.deepseek.com/v1".to_string()),
            key,
            "deepseek-v4-flash",
        )
    } else {
        (
            awaken_provider_genai::AdapterKind::OpenAI,
            None,
            std::env::var("OPENAI_API_KEY").expect("set OPENAI_API_KEY or DEEPSEEK_API_KEY"),
            "gpt-4o-mini",
        )
    };
    let model = std::env::var("AWAKEN_GENAI_MODEL").unwrap_or_else(|_| default_model.to_string());
    (GenaiExecutor::from_resolved(adapter, base_url, key), model)
}

fn text_request(model: String, prompt: &str) -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "live".to_string(),
            model_ref: model,
            backend_ref: "genai".to_string(),
        },
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text(prompt)],
        }],
        tools: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "requires network and a provider API key"]
async fn live_text_completion() {
    let (executor, model) = live_executor();
    let request = text_request(model, "Reply with the single word: pong");

    let response = executor.infer(request).await.expect("live inference");
    assert!(
        response.output.tool_calls().is_empty(),
        "expected text, got tool calls"
    );
    assert!(!response.output.text_content().is_empty());
}

#[tokio::test]
#[ignore = "requires network and a provider API key"]
async fn live_streaming_emits_public_or_reasoning_delta_before_returning_the_committed_response() {
    // Cause/effect graph: C1 a live OpenAI-compatible provider supports SSE;
    // C2 the selected model emits public text and may emit private reasoning;
    // C3 no tools are offered. Effects: E1 infer_streaming invokes DeltaSink at
    // least once before returning; E2 public streamed text reconstructs the
    // committed public answer; E3 tool deltas remain absent. Constraint: the
    // provider adapter may expose reasoning to the neutral runtime sink, while
    // answer-facing protocol adapters remain responsible for hiding its bytes.
    //
    // | Rule | SSE | public/reasoning delta | tools offered | Effects |
    // | R1 | yes | at least one | no | E1+E2+E3 |
    // | R2 | no/buffered | none | no | fail E1 |
    let (executor, model) = live_executor();
    let request = text_request(
        model,
        "Write one short Chinese sentence welcoming a user to a design studio.",
    );
    let deltas = RecordingDeltas::default();

    let response = executor
        .infer_streaming(request, &deltas)
        .await
        .expect("live streaming inference");
    let streamed_text = deltas.text.lock().unwrap().clone();
    let streamed_reasoning = deltas.reasoning.lock().unwrap().clone();
    eprintln!(
        "live delta lengths: public={} reasoning={}",
        streamed_text.len(),
        streamed_reasoning.len()
    );

    assert!(
        !streamed_text.is_empty() || !streamed_reasoning.is_empty(),
        "R1/E1 provider returned a committed response without any live delta"
    );
    assert_eq!(streamed_text, response.output.text_content(), "R1/E2");
    assert!(deltas.tool_arguments.lock().unwrap().is_empty(), "R1/E3");
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_anthropic_messages_completion() {
    let (executor, model) = live_deepseek_anthropic_executor();

    assert_text_completion(&executor, model).await;
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_openai_responses_completion() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY");
    // DeepSeek currently rejects its pro model on the Responses surface while
    // accepting it on Chat and Anthropic Messages. Keep the protocol live proof
    // on the strongest model the endpoint currently advertises as executable.
    let model =
        std::env::var("AWAKEN_RESPONSES_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
    let executor = OpenAiResponsesExecutor::new("https://api.deepseek.com/v1", key)
        .expect("construct Responses executor");

    assert_text_completion(&executor, model).await;
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_anthropic_messages_streaming_binds_signed_thinking_in_order() {
    // Cause/effect decision rule S1: split Anthropic thinking/signature deltas
    // plus a following tool-use block -> live reasoning/tool deltas and one
    // committed response whose signed Thinking precedes ToolUse. The terminal response,
    // not the transient deltas, is the continuation authority.
    // Constraints/invariants: signature and provider block order survive the
    // streaming boundary; live deltas cannot replace committed truth.
    let (executor, model) = live_deepseek_anthropic_executor();
    let deltas = RecordingDeltas::default();
    let response = executor
        .infer_streaming(
            ChatRequest {
                model_binding: ModelBinding {
                    provider_identity_ref: "deepseek".into(),
                    model_ref: model,
                    backend_ref: "genai".into(),
                },
                inference: Default::default(),
                messages: vec![compatibility_user("anthropic-stream")],
                tools: vec![compatibility_tool()],
            },
            &deltas,
        )
        .await
        .expect("S1 streaming signed thinking tool request");

    assert!(
        !deltas.reasoning.lock().unwrap().is_empty(),
        "S1/reasoning delta"
    );
    assert!(
        !deltas.tool_arguments.lock().unwrap().is_empty(),
        "S1/tool delta"
    );
    let thinking_index = response
        .output
        .blocks
        .iter()
        .position(|block| {
            matches!(
                block,
                ContentBlock::Thinking {
                    signature: Some(signature),
                    ..
                } if !signature.is_empty()
            )
        })
        .expect("S1 committed signed thinking");
    let tool_index = response
        .output
        .blocks
        .iter()
        .position(|block| matches!(block, ContentBlock::ToolUse { .. }))
        .expect("S1 committed tool use");
    assert!(
        thinking_index < tool_index,
        "S1 preserves provider block order"
    );
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_anthropic_messages_signed_thinking_tool_round_trip() {
    // Cause/effect decision rules: A1 reachable Anthropic Messages + Pro
    // reasoning + requested tool -> ordered signed Thinking and one ToolUse;
    // A2 replay that exact assistant message + correlated result -> accepted final
    // answer containing the fixture marker. A missing signature, reordered message,
    // or string-only reasoning path must fail A1 or make A2 fail at the provider.
    // Constraints/invariants: signed Thinking stays immediately bound to the
    // assistant ToolUse and the correlated result replays that exact message.
    let (executor, model) = live_deepseek_anthropic_executor();

    assert_deepseek_reasoning_tool_round_trip(
        &executor,
        model,
        "anthropic-messages",
        "ANTHROPIC_MESSAGES_ROUND_TRIP_OK",
        true,
    )
    .await;
}

fn live_deepseek_anthropic_executor() -> (GenaiExecutor, String) {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY");
    let model =
        std::env::var("AWAKEN_ANTHROPIC_MODEL").unwrap_or_else(|_| "deepseek-v4-pro".to_string());
    (
        GenaiExecutor::from_resolved(
            awaken_provider_genai::AdapterKind::Anthropic,
            Some("https://api.deepseek.com/anthropic".to_string()),
            key,
        ),
        model,
    )
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_openai_chat_reasoning_tool_round_trip() {
    // Cause/effect graph: C1 DeepSeek OpenAI Chat is reachable; C2 the Pro model
    // emits reasoning; C3 one model-visible tool is offered and explicitly
    // requested; C4 the correlated tool result is returned with the complete
    // assistant message. Effects: E1 receive Thinking+ToolUse; E2 preserve typed
    // tool identity/arguments; E3 the follow-up is accepted and produces public
    // text. Constraint: tool_choice is omitted (`auto`) because DeepSeek V4
    // thinking rejects forced/required selection.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effects |
    // | R1   | Y  | Y  | Y  | -  | E1+E2   |
    // | R2   | Y  | Y  | Y  | Y  | E3      |
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY");
    let model = std::env::var("AWAKEN_DEEPSEEK_CHAT_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-pro".to_string());
    let executor = GenaiExecutor::from_resolved(
        awaken_provider_genai::AdapterKind::OpenAI,
        Some("https://api.deepseek.com/v1".to_string()),
        key,
    );

    assert_deepseek_reasoning_tool_round_trip(
        &executor,
        model,
        "openai-chat",
        "OPENAI_CHAT_ROUND_TRIP_OK",
        false,
    )
    .await;
}

async fn assert_deepseek_reasoning_tool_round_trip(
    executor: &GenaiExecutor,
    model: String,
    requested_marker: &str,
    result_marker: &str,
    require_signature: bool,
) {
    let tool = compatibility_tool();
    let user = compatibility_user(requested_marker);
    let first = executor
        .infer(ChatRequest {
            model_binding: ModelBinding {
                provider_identity_ref: "deepseek".into(),
                model_ref: model.clone(),
                backend_ref: "genai".into(),
            },
            inference: Default::default(),
            messages: vec![user.clone()],
            tools: vec![tool.clone()],
        })
        .await
        .expect("R1 DeepSeek reasoning tool request");

    let calls = first.output.tool_calls();
    assert_eq!(calls.len(), 1, "R1/E1 one typed tool call");
    assert_eq!(calls[0].tool_id, "read_compatibility_fixture", "R1/E2");
    assert_eq!(calls[0].arguments["marker"], requested_marker, "R1/E2");
    assert!(
        first
            .output
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking { text, .. } if !text.is_empty())),
        "R1/E1 reasoning must be retained for the continuation"
    );
    if require_signature {
        assert!(
            first.output.blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Thinking {
                    signature: Some(signature),
                    ..
                } if !signature.is_empty()
            )),
            "R1/E1 Anthropic thinking must retain its replay signature"
        );
    }

    let second = executor
        .infer(ChatRequest {
            model_binding: ModelBinding {
                provider_identity_ref: "deepseek".into(),
                model_ref: model,
                backend_ref: "genai".into(),
            },
            inference: Default::default(),
            messages: vec![
                user,
                ChatMessage {
                    role: Role::Assistant,
                    content: first.output.blocks,
                },
                ChatMessage {
                    role: Role::Tool,
                    content: vec![ContentBlock::tool_result(
                        calls[0].call_id.clone(),
                        vec![ContentBlock::text(result_marker)],
                    )],
                },
            ],
            tools: vec![tool],
        })
        .await
        .expect("R2 DeepSeek reasoning and tool result continuation");

    assert!(
        second.output.text_content().contains(result_marker),
        "R2/E3 final answer must use the returned tool result"
    );
}

fn compatibility_user(requested_marker: &str) -> ChatMessage {
    ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "Call read_compatibility_fixture exactly once with marker `{requested_marker}`, then report its result. Do not guess the result."
        ))],
    }
}

async fn assert_text_completion(executor: &dyn LlmExecutor, model: String) {
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "deepseek".to_string(),
            model_ref: model,
            backend_ref: "genai".to_string(),
        },
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("Reply with the single word: pong")],
        }],
        tools: Vec::new(),
    };

    let response = executor.infer(request).await.expect("live inference");
    assert!(
        response.output.tool_calls().is_empty(),
        "expected text, got tool calls"
    );
    assert!(!response.output.text_content().is_empty());
}
