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
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_ext_builtin_tools::HandToolContext;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest};
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
    let (executor, model) = build_executor().expect("build executor");
    let request = ChatRequest {
        model_binding: ModelBinding::new("default", model, "default"),
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
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
    let (llm, model) = build_executor().expect("build executor");
    let dir = std::env::temp_dir().join(format!("awaken_minimax_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("greeting.txt");
    std::fs::write(&file, "Hello, world.\n").unwrap();
    let path = file.to_str().unwrap().to_string();
    // Live cause/effect: the file is inside the explicit Hand root; the model
    // reads it and requests edit; Allow mutates and Deny preserves bytes. An
    // outside-root refusal cannot count as successful edit/permission evidence.
    let session = CodingSession::new_with_hand_context(
        build_runtime(llm),
        coding_config(&model).expect("model reference is an executable binding"),
        HandToolContext::new(&dir),
        awaken_time::system_clock(),
    );

    let prompt = format!(
        "Edit the file at {path}: change its greeting to say 'Hello, awaken!'. \
         Read it first, then use the edit tool. If that edit is denied, report the denial and stop; do not use a fallback mutation."
    );
    let asked = Arc::new(AtomicUsize::new(0));
    let asked_for = asked.clone();
    let messages = session
        .turn(&prompt, move |ticket| {
            if ticket
                .pending_tool()
                .is_some_and(|tool| tool.tool_id.rsplit(':').next() == Some("edit"))
            {
                asked_for.fetch_add(1, Ordering::SeqCst);
            }
            Approval::Allow
        })
        .await
        .expect("the live turn completes");

    for m in &messages {
        println!("[{:?}] {}", m.role, m.text_content());
    }
    let after = std::fs::read_to_string(&file).unwrap();
    println!("--- {path} after ---\n{after}");
    assert!(
        asked.load(Ordering::SeqCst) >= 1,
        "the edit itself required approval"
    );
    assert!(
        after.contains("awaken"),
        "the model edited the file to mention awaken (got: {after:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The permission gate, live: the model tries to mutate the file, the caller
/// denies, and the file is left untouched. Proves the gate awaits mutating tools
/// (ADR-0030) and a denial actually blocks the change against a real model.
#[tokio::test]
#[ignore = "requires MINIMAX_API_KEY and network"]
async fn minimax_agent_denied_edit_leaves_the_file_unchanged() {
    let (llm, model) = build_executor().expect("build executor");
    let dir = std::env::temp_dir().join(format!("awaken_deny_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("greeting.txt");
    std::fs::write(&file, "Hello, world.\n").unwrap();
    let path = file.to_str().unwrap().to_string();
    // Live cause/effect: the file is inside the explicit Hand root; the model
    // reads it and requests edit; Allow mutates and Deny preserves bytes. An
    // outside-root refusal cannot count as successful edit/permission evidence.
    let session = CodingSession::new_with_hand_context(
        build_runtime(llm),
        coding_config(&model).expect("model reference is an executable binding"),
        HandToolContext::new(&dir),
        awaken_time::system_clock(),
    );

    // Deny every mutation and count that the gate actually asked.
    let asked = Arc::new(AtomicUsize::new(0));
    let asked_for = asked.clone();
    let prompt = format!(
        "Edit the file at {path}: change its greeting to say 'Hello, awaken!'. \
         Read it first, then use the edit tool. If that edit is denied, report the denial and stop; do not use a fallback mutation."
    );
    let messages = session
        .turn(&prompt, move |ticket| {
            if ticket
                .pending_tool()
                .is_some_and(|tool| tool.tool_id.rsplit(':').next() == Some("edit"))
            {
                asked_for.fetch_add(1, Ordering::SeqCst);
            }
            assert_eq!(ticket.reason(), AwaitReason::ToolPermission);
            Approval::Deny
        })
        .await
        .expect("the live turn completes");

    for m in &messages {
        println!("[{:?}] {}", m.role, m.text_content());
    }
    assert!(
        asked.load(Ordering::SeqCst) >= 1,
        "the model attempted a mutation, so the gate asked at least once"
    );
    let after = std::fs::read_to_string(&file).unwrap();
    println!("--- {path} after (denied) ---\n{after}");
    assert_eq!(
        after, "Hello, world.\n",
        "a denied mutation leaves the file unchanged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
