//! Live provider smoke test. Ignored by default; it needs network and a
//! provider API key in the environment (e.g. `OPENAI_API_KEY`). Run with:
//!
//! ```sh
//! AWAKEN_GENAI_MODEL=gpt-4o-mini cargo test -p awaken-provider-genai --test live -- --ignored
//! ```

use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatContent, ChatMessage, ChatRequest, ChatRole, LlmExecutor,
};
use awaken_runtime_contract::resolved::ModelBinding;

#[tokio::test]
#[ignore = "requires network and a provider API key"]
async fn live_text_completion() {
    let model = std::env::var("AWAKEN_GENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let executor = GenaiExecutor::new();

    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_instance_ref: "live".to_string(),
            model_ref: model,
            backend_ref: "genai".to_string(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: ChatContent::Text("Reply with the single word: pong".to_string()),
        }],
        tools: Vec::new(),
    };

    let response = executor.infer(request).await.expect("live inference");
    match response.output {
        AssistantOutput::Text(text) => assert!(!text.is_empty()),
        AssistantOutput::ToolCalls(_) => panic!("expected text, got tool calls"),
    }
}
