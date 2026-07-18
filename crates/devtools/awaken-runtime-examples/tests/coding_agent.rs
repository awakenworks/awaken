//! Offline smoke test for the coding agent: a scripted model reads a real file,
//! edits it, and the edit awaits for approval before it runs. Proves the agent
//! actually mutates code and that the permission gate (ADR-0030) gates mutations
//! — no network, no API key.
#![cfg(feature = "coding-agent")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_examples::coding_agent::{
    Approval, CodingSession, ScriptedCoder, build_runtime, coding_config,
};

#[tokio::test]
async fn agent_reads_then_edits_a_file_after_approval() {
    // A real file in a temp dir, containing the text the agent will replace.
    let dir = std::env::temp_dir().join(format!("awaken_coding_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.txt");
    std::fs::write(&file, "status: TODO\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    // Scripted model: read the file, then edit TODO -> DONE, then report.
    let llm = Arc::new(ScriptedCoder::new(path.clone(), "TODO", "DONE"));
    let runtime = build_runtime(llm);
    let session = CodingSession::new(runtime, coding_config("scripted-model"));

    // Count approval prompts: `read` is allowed (no prompt); `edit` is asked.
    let asked = Arc::new(AtomicUsize::new(0));
    let asked_for = asked.clone();
    let new_messages = session
        .turn("Mark the status done.", move |ticket| {
            asked_for.fetch_add(1, Ordering::SeqCst);
            assert_eq!(ticket.reason, AwaitReason::ToolPermission);
            assert_eq!(ticket.call_id.as_deref(), Some("edit-1"));
            Approval::Allow
        })
        .await
        .expect("turn runs");

    // The edit awaiting for approval exactly once (read did not).
    assert_eq!(asked.load(Ordering::SeqCst), 1, "only the edit was asked");

    // The file was actually mutated.
    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(after, "status: DONE\n", "the edit was applied");

    // The committed transcript holds the user turn, the tool results, and the
    // assistant's closing message.
    assert_eq!(new_messages.first().unwrap().role, Role::User);
    assert!(
        new_messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("status: TODO")),
        "the read tool result is committed"
    );
    assert!(
        new_messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text_content().contains("DONE")),
        "the assistant reported the edit"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_denied_edit_does_not_mutate_the_file() {
    let dir = std::env::temp_dir().join(format!("awaken_coding_deny_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.txt");
    std::fs::write(&file, "status: TODO\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    let llm = Arc::new(ScriptedCoder::new(path.clone(), "TODO", "DONE"));
    let runtime = build_runtime(llm);
    let session = CodingSession::new(runtime, coding_config("scripted-model"));

    session
        .turn("Mark the status done.", |_ticket| Approval::Deny)
        .await
        .expect("turn runs");

    // Denied: the file is untouched.
    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        after, "status: TODO\n",
        "a denied edit leaves the file alone"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
