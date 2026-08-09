//! Live provider smoke test. Ignored by default; it needs network and a
//! an explicit OpenAI-compatible provider API key in the environment. This test reads it itself and
//! injects it into a fixed adapter; the provider adapter has no ambient default.
//! Run with:
//!
//! ```sh
//! AWAKEN_GENAI_MODEL=gpt-4o-mini cargo test -p awaken-provider-genai --test live -- --ignored
//! ```

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, DeltaSink, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use std::sync::Mutex;

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
async fn live_streaming_emits_public_or_reasoning_delta_before_returning_the_committed_turn() {
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
        "R1/E1 provider returned a committed turn without any live delta"
    );
    assert_eq!(streamed_text, response.output.text_content(), "R1/E2");
    assert!(deltas.tool_arguments.lock().unwrap().is_empty(), "R1/E3");
}
