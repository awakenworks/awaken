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

use awaken_runtime_examples::coding_agent::model::build_executor;
use awaken_runtime_examples::coding_agent::{
    Approval, CodingSession, build_runtime, coding_config,
};

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
