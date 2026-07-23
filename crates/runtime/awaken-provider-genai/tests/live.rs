//! Live provider smoke test. Ignored by default; it needs network and a
//! an explicit OpenAI API key in the environment. This test reads it itself and
//! injects it into a fixed adapter; the provider adapter has no ambient default.
//! Run with:
//!
//! ```sh
//! AWAKEN_GENAI_MODEL=gpt-4o-mini cargo test -p awaken-provider-genai --test live -- --ignored
//! ```

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;

#[tokio::test]
#[ignore = "requires network and a provider API key"]
async fn live_text_completion() {
    let model = std::env::var("AWAKEN_GENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let key = std::env::var("OPENAI_API_KEY").expect("set OPENAI_API_KEY");
    let executor =
        GenaiExecutor::from_resolved(awaken_provider_genai::AdapterKind::OpenAI, None, key);

    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "live".to_string(),
            model_ref: model,
            backend_ref: "genai".to_string(),
        },
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
