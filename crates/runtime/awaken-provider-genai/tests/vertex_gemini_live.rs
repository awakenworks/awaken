//! Live Gemini-on-Vertex probe via an OAuth2 Bearer token (ADR-0043 Phase 3
//! multi-dialect + OAuth). Ignored by default; needs network + a Google OAuth2
//! access token with the cloud-platform scope. Run with:
//!
//! ```sh
//! GEMINI_PROJECT=my-proj GEMINI_LOCATION=global GEMINI_MODEL=gemini-2.5-flash \
//! GEMINI_ACCESS_TOKEN=$(gcloud auth print-access-token) \
//! cargo test -p awaken-provider-genai --test vertex_gemini_live -- --ignored --nocapture
//! ```

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;

#[tokio::test]
#[ignore = "requires network and a Google OAuth2 access token"]
async fn gemini_on_vertex_with_oauth_bearer() {
    let project = std::env::var("GEMINI_PROJECT").expect("set GEMINI_PROJECT");
    let location = std::env::var("GEMINI_LOCATION").unwrap_or_else(|_| "global".to_string());
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
    let token = std::env::var("GEMINI_ACCESS_TOKEN").expect("set GEMINI_ACCESS_TOKEN (OAuth2)");

    let executor = GenaiExecutor::vertex_gemini(project, location, token);
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "vertex".into(),
            model_ref: model,
            backend_ref: "genai".into(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text(
                "Reply with exactly the single word: pong",
            )],
        }],
        tools: Vec::new(),
    };
    let response = executor
        .infer(request)
        .await
        .expect("live Gemini inference");
    let text = response.output.text_content();
    assert!(!text.is_empty(), "Gemini returned non-empty text");
    eprintln!("Gemini/Vertex replied: {text:?}");
}
