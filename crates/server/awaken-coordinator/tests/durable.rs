//! Level 0 — durable pause/resume across a restart.
//!
//! A run awaits on a client-executed tool and its `SharedHost` is dropped
//! (simulating a process exit). A brand-new host built over the *same* store
//! directory recovers the awaiting position from committed truth and resumes the
//! run to completion — proving the pause survives a restart when the commit
//! boundary is durable (SQLite), not in-memory.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_coordinator::{HostResume, SharedHost};
use awaken_scenario_host::CustomToolModel;

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

async fn host_over(dir: &std::path::Path) -> SharedHost {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.storage_dir = Some(dir.to_path_buf());
    let authority = awaken_coordinator::init_scenario_runtime(&deployment)
        .await
        .expect("open Coordinator-owned SQLite runtime authority");
    SharedHost::new(Arc::new(CustomToolModel), "custom")
        .with_client_tools(client_tools)
        .with_store_dir(dir.to_path_buf())
        .with_runtime_authority(authority)
}

#[tokio::test]
async fn awaiting_run_survives_a_restart_and_resumes_from_the_durable_store() {
    let dir = std::env::temp_dir().join(format!("awaken-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let thread = "durable-1";

    // Restart-recovery FMECA / cause-effect decision table:
    // C1=Coordinator injects an authority, C2=the replacement opens the same
    // durable root. R1 C1+C2 -> awaiting ticket and history recover; R2 !C1 ->
    // fail closed/no implicit Host store; R3 C1+!C2 -> clean independent state.
    // This case proves R1 end to end; the in-memory case below proves the R2/R3
    // non-recovery effect without reintroducing a second Host-owned store path.
    // 1. First "process": run a turn that awaits on the client tool, then drop the
    //    host — the run's history and awaiting ticket are now only in the store.
    let pending_id = {
        let host = host_over(&dir).await;
        host.run(None, thread, vec![user("u1", "hi")])
            .await
            .unwrap();
        assert!(
            host.is_awaiting(thread).await,
            "the run should await on the client-executed tool"
        );
        let pending = host
            .pending_tool(thread)
            .await
            .expect("the durable pending-tool read succeeds")
            .expect("an awaiting run exposes its pending tool");
        assert_eq!(pending.name, "submit_answer");
        assert!(pending.client_executed);
        assert!(
            !host.committed_messages(thread).await.is_empty(),
            "the turn is committed to durable truth before the restart"
        );
        pending.tool_use_id
    };

    // 2. A brand-new host over the SAME store directory recovers the awaiting run.
    let host = host_over(&dir).await;
    assert!(
        host.is_awaiting(thread).await,
        "the rebuilt host recovers the awaiting run from the durable store"
    );
    assert!(
        !host.committed_messages(thread).await.is_empty(),
        "the committed history is readable after the restart"
    );

    // 3. RunResume it with the client's result; the model replies with the result.
    host.resume(
        thread,
        &pending_id,
        HostResume::ClientResult {
            content: vec![ContentBlock::text("42")],
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
async fn in_memory_host_does_not_recover_an_awaiting_run_across_a_rebuild() {
    let thread = "ephemeral-1";
    let client_tools = HashSet::from(["submit_answer".to_string()]);

    {
        let host = SharedHost::new(Arc::new(CustomToolModel), "custom")
            .with_client_tools(client_tools.clone());
        host.run(None, thread, vec![user("u1", "hi")])
            .await
            .unwrap();
        assert!(host.is_awaiting(thread).await);
    }

    let host = SharedHost::new(Arc::new(CustomToolModel), "custom").with_client_tools(client_tools);
    assert!(
        !host.is_awaiting(thread).await,
        "an in-memory host starts clean; the awaiting run does not survive"
    );
}
