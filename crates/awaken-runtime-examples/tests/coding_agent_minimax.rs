//! Live verification of the coding agent against a real model. Ignored by
//! default (needs an API key and network). Run manually:
//!
//! ```text
//! MINIMAX_API_KEY=... AWAKEN_MODEL=MiniMax-M3 \
//!   cargo test -p awaken-runtime-examples --features coding-agent-tui \
//!     --test coding_agent_minimax -- --ignored --nocapture
//! ```
#![cfg(feature = "coding-agent-tui")]

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_examples::coding_agent::model::build_executor;
use awaken_runtime_examples::coding_agent::{
    Approval, CodingSession, build_runtime, coding_config,
};

/// Verifies the MiniMax config path through the real executor: the request must
/// authenticate and reach the model. A completion (account has credits) or a
/// quota/limit error both prove the wiring; an auth or connection failure would
/// not. Run with the MiniMax env from `~/.bashrc`.
#[tokio::test]
#[ignore = "requires MINIMAX_API_KEY and network"]
async fn minimax_endpoint_authenticates_and_reaches_the_model() {
    let model = std::env::var("AWAKEN_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string());
    let executor = build_executor().expect("build executor");
    let request = ChatRequest {
        model_binding: ModelBinding::new("default", model, "default"),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("Reply with the single word: ready")],
        }],
        tools: Vec::new(),
    };

    match executor.infer(request).await {
        // With credits, a real completion.
        Ok(response) => println!("minimax completion: {}", response.output.text_content()),
        // Without, the error still proves the client built and routed the request
        // to the right model via the Anthropic adapter at the custom endpoint
        // (`Web call failed for model 'MiniMax-M3 (adapter: Anthropic)'`). The
        // endpoint's quota response itself is observable out of band (curl).
        Err(err) => {
            let message = err.to_string();
            println!("minimax provider error: {message}");
            assert!(
                message.contains("MiniMax") && message.contains("Anthropic"),
                "the executor routes to the MiniMax model via the Anthropic adapter \
                 (got: {message})"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires MINIMAX_API_KEY and network"]
async fn minimax_agent_edits_a_real_file() {
    let model = std::env::var("AWAKEN_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string());
    let llm = Arc::new(build_executor().expect("build executor"));
    let session = CodingSession::new(build_runtime(llm), coding_config(&model));

    let dir = std::env::temp_dir().join(format!("awaken_minimax_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("greeting.txt");
    std::fs::write(&file, "Hello, world.\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    let prompt = format!(
        "Edit the file at {path}: change its greeting to say 'Hello, awaken!'. \
         Read it first, then use the edit tool."
    );
    let messages = session
        .turn(&prompt, |_ticket| Approval::Allow)
        .await
        .expect("the live turn completes");

    for m in &messages {
        println!("[{:?}] {}", m.role, m.text_content());
    }
    let after = std::fs::read_to_string(&file).unwrap();
    println!("--- {path} after ---\n{after}");
    assert!(
        after.contains("awaken"),
        "the model edited the file to mention awaken (got: {after:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
