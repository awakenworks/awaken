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
            "https://api.deepseek.com/v1".to_string(),
            key,
            "deepseek-v4-flash",
        )
    } else {
        (
            awaken_provider_genai::AdapterKind::OpenAI,
            "https://api.openai.com/v1".to_string(),
            std::env::var("OPENAI_API_KEY").expect("set OPENAI_API_KEY or DEEPSEEK_API_KEY"),
            "gpt-4o-mini",
        )
    };
    let model = std::env::var("AWAKEN_GENAI_MODEL").unwrap_or_else(|_| default_model.to_string());
    (
        GenaiExecutor::from_materialized_endpoint(adapter, base_url, key),
        model,
    )
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
    let executor = OpenAiResponsesExecutor::from_materialized_endpoint(
        "deepseek",
        "https://api.deepseek.com/v1",
        key,
    )
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
        GenaiExecutor::from_materialized_endpoint(
            awaken_provider_genai::AdapterKind::Anthropic,
            "https://api.deepseek.com/anthropic",
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
    let executor = GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::OpenAI,
        "https://api.deepseek.com/v1",
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

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_openai_chat_three_large_tool_rounds() {
    // Multi-round compatibility cause/effect table. Causes: C1 protocol is
    // OpenAI Chat or Anthropic Messages; C2 transport is buffered or SSE; C3
    // three correlated tool results approximate the observed GenerationContext,
    // compact Skill, and SubjectPack sizes; C4 40 unrelated tools remain
    // advertised. Effects: E1 each continuation is usable and contains the one
    // ordered ToolUse; E2 SSE emits the same tool/text payload that becomes the
    // committed response; E3 the terminal response contains MULTI_ROUND_OK; E4
    // an empty provider choice is a protocol failure, never a successful empty
    // answer. Decision rules: M1 each C1 x C2 pairing+C3+C4=>E1; M2 SSE=>E2;
    // M3 all three results=>E3; M4 empty choice at any round=>E4. The exact
    // assistant messages remain replay authority; tests never flatten or invent
    // reasoning blocks.
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY");
    let model = std::env::var("AWAKEN_DEEPSEEK_CHAT_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-pro".to_string());
    let executor = GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::OpenAI,
        "https://api.deepseek.com/v1",
        key,
    );

    assert_deepseek_three_large_tool_rounds(&executor, model, TestTransport::Buffered).await;
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_openai_chat_streaming_three_large_tool_rounds() {
    // Covers M1-M4 with OpenAI Chat + SSE, the exact transport pairing used by
    // Managed Run execution. This is intentionally distinct from the buffered
    // rule because provider stream assembly owns the empty-choice failure.
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY");
    let model = std::env::var("AWAKEN_DEEPSEEK_CHAT_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-pro".to_string());
    let executor = GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::OpenAI,
        "https://api.deepseek.com/v1",
        key,
    );

    assert_deepseek_three_large_tool_rounds(&executor, model, TestTransport::Streaming).await;
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_anthropic_messages_three_large_tool_rounds() {
    // Same M1-M3 decision rules as the OpenAI Chat proof. Running the identical
    // transcript shape through Anthropic Messages isolates protocol compatibility
    // from model, tool schema, and result-size causes.
    let (executor, model) = live_deepseek_anthropic_executor();
    assert_deepseek_three_large_tool_rounds(&executor, model, TestTransport::Buffered).await;
}

#[tokio::test]
#[ignore = "requires network and DEEPSEEK_API_KEY"]
async fn live_deepseek_anthropic_messages_streaming_three_large_tool_rounds() {
    // Covers M1-M4 with Anthropic Messages + SSE. Together with the OpenAI Chat
    // rule, this distinguishes a provider/model failure from an adapter-specific
    // streaming continuation failure.
    let (executor, model) = live_deepseek_anthropic_executor();
    assert_deepseek_three_large_tool_rounds(&executor, model, TestTransport::Streaming).await;
}

#[derive(Clone, Copy)]
enum TestTransport {
    Buffered,
    Streaming,
}

async fn assert_deepseek_three_large_tool_rounds(
    executor: &GenaiExecutor,
    model: String,
    transport: TestTransport,
) {
    let tool = compatibility_tool();
    let mut tools = vec![tool.clone()];
    for index in 0..40 {
        let mut unrelated = tool.clone();
        unrelated.id = format!("unused_browser_fixture_{index}");
        unrelated.description = format!(
            "Unrelated browser capability {index}. {}",
            "Describe viewport, accessibility, navigation, input, output, and recovery fields. "
                .repeat(12)
        );
        tools.push(unrelated);
    }
    let mut messages = vec![ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::text(
            "Call read_compatibility_fixture exactly three times in sequence with markers round-1, round-2, and round-3. Wait for each result before the next call. After round-3, reply with MULTI_ROUND_OK.",
        )],
    }];
    let result_lengths = [771_usize, 10_106, 3_910];

    for (index, result_length) in result_lengths.into_iter().enumerate() {
        let response = infer_for_test_transport(
            executor,
            ChatRequest {
                model_binding: ModelBinding {
                    provider_identity_ref: "deepseek".into(),
                    model_ref: model.clone(),
                    backend_ref: "genai".into(),
                },
                inference: Default::default(),
                messages: messages.clone(),
                tools: tools.clone(),
            },
            transport,
        )
        .await
        .unwrap_or_else(|error| panic!("M1/E1 or M4/E4 usable continuation: {error}"));
        let calls = response.output.tool_calls();
        assert_eq!(calls.len(), 1, "M1/E1 one ToolUse at round {}", index + 1);
        assert_eq!(
            calls[0].arguments["marker"],
            format!("round-{}", index + 1),
            "M1/E1 ordered marker at round {}",
            index + 1
        );
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: response.output.blocks,
        });
        messages.push(ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result(
                calls[0].call_id.clone(),
                vec![ContentBlock::text(format!(
                    "ROUND_{}_RESULT:{}",
                    index + 1,
                    "x".repeat(result_length)
                ))],
            )],
        });
    }

    let terminal = infer_for_test_transport(
        executor,
        ChatRequest {
            model_binding: ModelBinding {
                provider_identity_ref: "deepseek".into(),
                model_ref: model,
                backend_ref: "genai".into(),
            },
            inference: Default::default(),
            messages,
            tools,
        },
        transport,
    )
    .await
    .expect("M3/E3 terminal marker or M4/E4 fail closed");
    assert!(
        terminal.output.text_content().contains("MULTI_ROUND_OK"),
        "M3/E3 completion marker"
    );
}

async fn infer_for_test_transport(
    executor: &GenaiExecutor,
    request: ChatRequest,
    transport: TestTransport,
) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
    match transport {
        TestTransport::Buffered => executor.infer(request).await,
        TestTransport::Streaming => {
            let deltas = RecordingDeltas::default();
            let response = executor.infer_streaming(request, &deltas).await?;
            let calls = response.output.tool_calls();
            if calls.is_empty() {
                assert_eq!(
                    deltas.text.lock().unwrap().as_str(),
                    response.output.text_content(),
                    "M2/E2 streamed text is committed text"
                );
            } else {
                assert!(
                    !deltas.tool_arguments.lock().unwrap().is_empty(),
                    "M2/E2 streamed tool arguments are observable"
                );
            }
            Ok(response)
        }
    }
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
