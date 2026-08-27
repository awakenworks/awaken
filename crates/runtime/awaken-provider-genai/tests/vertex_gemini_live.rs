//! Live Gemini-on-Vertex probe via an OAuth2 Bearer token (ADR-0043 Phase 3
//! multi-dialect + OAuth). Ignored by default; needs network + a Google OAuth2
//! access token with the cloud-platform scope. Run with:
//!
//! ```sh
//! GEMINI_PROJECT=my-proj GEMINI_LOCATION=global GEMINI_MODEL=gemini-2.5-flash \
//! GEMINI_ACCESS_TOKEN=$(gcloud auth print-access-token) \
//! cargo test -p awaken-provider-genai --test vertex_gemini_live -- --ignored --nocapture
//! ```

mod support;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use support::compatibility_tool;

fn live_executor() -> GenaiExecutor {
    let project = std::env::var("GEMINI_PROJECT").expect("set GEMINI_PROJECT");
    let location = std::env::var("GEMINI_LOCATION").unwrap_or_else(|_| "global".to_string());
    let token = std::env::var("GEMINI_ACCESS_TOKEN").expect("set GEMINI_ACCESS_TOKEN (OAuth2)");
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::Vertex,
        format!("https://{host}/v1/projects/{project}/locations/{location}/"),
        token,
    )
}

#[tokio::test]
#[ignore = "requires network and a Google OAuth2 access token"]
async fn gemini_on_vertex_with_oauth_bearer() {
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
    let executor = live_executor();
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "vertex".into(),
            model_ref: model,
            backend_ref: "genai".into(),
        },
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
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

#[tokio::test]
#[ignore = "requires network and a Google OAuth2 access token"]
async fn gemini_on_vertex_accepts_generated_schema_and_completes_a_tool_round_trip() {
    // Cause/effect rules: G1 a typed Tool generates its own closed object schema
    // and is sent through Vertex's parametersJsonSchema field -> Gemini returns
    // the expected typed call. G2 replay the complete assistant turn followed by
    // the correlated tool result -> Gemini accepts the continuation and reports
    // the marker. Together they cover declaration and multi-turn compatibility.
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
    let executor = live_executor();
    let user = ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::text(
            "Call read_compatibility_fixture exactly once with marker `gcloud-gemini`, then report its result. Do not guess the result.",
        )],
    };
    let binding = ModelBinding {
        provider_identity_ref: "vertex".into(),
        model_ref: model,
        backend_ref: "genai".into(),
    };
    let tool = compatibility_tool();

    let first = executor
        .infer(ChatRequest {
            model_binding: binding.clone(),
            inference: Default::default(),
            messages: vec![user.clone()],
            tools: vec![tool.clone()],
        })
        .await
        .expect("G1 Gemini typed tool call");
    let calls = first.output.tool_calls();
    assert_eq!(calls.len(), 1, "G1 one tool call");
    assert_eq!(calls[0].tool_id, "read_compatibility_fixture", "G1 id");
    assert_eq!(calls[0].arguments["marker"], "gcloud-gemini", "G1 args");

    let marker = "GCLOUD_GEMINI_ROUND_TRIP_OK";
    let second = executor
        .infer(ChatRequest {
            model_binding: binding,
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
                        vec![ContentBlock::text(marker)],
                    )],
                },
            ],
            tools: vec![tool],
        })
        .await
        .expect("G2 Gemini tool-result continuation");
    assert!(second.output.text_content().contains(marker), "G2 marker");
}
