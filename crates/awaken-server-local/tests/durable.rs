//! Level 0 — durable pause/resume across a restart.
//!
//! A run parks on a client-executed tool and its `SharedHost` is dropped
//! (simulating a process exit). A brand-new host built over the *same* store
//! directory recovers the parked position from committed truth and resumes the
//! run to completion — proving the pause survives a restart when the commit
//! boundary is durable (SQLite), not in-memory.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_server_local::{CustomToolModel, HostResume, SharedHost};

fn user(id: &str, text: &str) -> Message {
    Message::text(MessageId(id.into()), Role::User, text)
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn host_over(dir: &std::path::Path) -> SharedHost {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    SharedHost::with_client_tools(Arc::new(CustomToolModel), "custom", client_tools)
        .with_store_dir(dir.to_path_buf())
}

#[tokio::test]
async fn parked_run_survives_a_restart_and_resumes_from_the_durable_store() {
    let dir = std::env::temp_dir().join(format!("awaken-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let thread = "durable-1";

    // 1. First "process": run a turn that parks on the client tool, then drop the
    //    host — the run's history and waiting ticket are now only in the store.
    let pending_id = {
        let host = host_over(&dir);
        host.run_turn(thread, vec![user("u1", "hi")]).await.unwrap();
        assert!(
            host.is_parked(thread).await,
            "the run should park on the client-executed tool"
        );
        let pending = host
            .pending_tool(thread)
            .await
            .expect("a parked run exposes its pending tool");
        assert_eq!(pending.name, "submit_answer");
        assert!(pending.client_executed);
        assert!(
            !host.committed_messages(thread).await.is_empty(),
            "the turn is committed to durable truth before the restart"
        );
        pending.tool_use_id
    };

    // 2. A brand-new host over the SAME store directory recovers the parked run.
    let host = host_over(&dir);
    assert!(
        host.is_parked(thread).await,
        "the rebuilt host recovers the parked run from the durable store"
    );
    assert!(
        !host.committed_messages(thread).await.is_empty(),
        "the committed history is readable after the restart"
    );

    // 3. Resume it with the client's result; the model replies with the result.
    host.resume(
        thread,
        &pending_id,
        HostResume::ClientResult {
            content: "42".into(),
            is_error: false,
        },
    )
    .await
    .unwrap();

    let history = host.committed_messages(thread).await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("got:"))
        .map(text_of)
        .expect("the resumed run commits the model's final reply");
    assert!(
        reply.contains("got: 42"),
        "the resumed run should incorporate the client's result: {reply:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The in-memory host (no store dir) does NOT recover across a rebuild: a new host
/// starts clean. This pins the durability contract to the store, not the type.
#[tokio::test]
async fn in_memory_host_does_not_recover_a_parked_run_across_a_rebuild() {
    let thread = "ephemeral-1";
    let client_tools = HashSet::from(["submit_answer".to_string()]);

    {
        let host = SharedHost::with_client_tools(
            Arc::new(CustomToolModel),
            "custom",
            client_tools.clone(),
        );
        host.run_turn(thread, vec![user("u1", "hi")]).await.unwrap();
        assert!(host.is_parked(thread).await);
    }

    let host = SharedHost::with_client_tools(Arc::new(CustomToolModel), "custom", client_tools);
    assert!(
        !host.is_parked(thread).await,
        "an in-memory host starts clean; the parked run does not survive"
    );
}
