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
use awaken_coordinator::{HostResume, ManagedHost, SharedHost};
use awaken_scenario_host::{
    CustomToolModel, build_unmounted_host, build_unmounted_host_with_deployment,
};

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

fn host_with_client_tool(host: SharedHost) -> Arc<SharedHost> {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let host = Arc::new(host.with_client_tools(client_tools));
    let _runtime = ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    host
}

async fn host_over(dir: &std::path::Path, process: &str) -> Arc<SharedHost> {
    let mut authority_deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    authority_deployment.storage_dir = Some(dir.to_path_buf());
    let authority = awaken_coordinator::init_scenario_runtime(
        &authority_deployment,
        awaken_time::system_clock(),
    )
    .await
    .expect("open Coordinator-owned SQLite runtime authority");
    let mut worker_deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    worker_deployment.sandbox_dir = Some(dir.join(format!("sandbox-{process}")));
    host_with_client_tool(
        Arc::into_inner(build_unmounted_host_with_deployment(
            Arc::new(CustomToolModel),
            "custom",
            worker_deployment,
        ))
        .expect("unmounted scenario host has one owner")
        .with_runtime_authority(authority),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaiting_run_survives_a_restart_and_resumes_from_the_durable_store() {
    // Causes: the fixtures below establish `awaiting run survives a restart and` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `resumes from the durable store` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let dir = std::env::temp_dir().join(format!("awaken-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let thread = "durable-1";

    // Both durable and ephemeral cases use the same Resources-complete Scenario
    // composition. Empty artifact capture must succeed before the tested pause;
    // only the injected runtime authority determines restart recovery.
    // Restart-recovery FMECA / cause-effect decision table:
    // C1=Coordinator injects an authority and the canonical dispatch Session
    // Runtime, C2=the replacement opens the same durable root, C3=each simulated
    // process owns a distinct process-local sandbox root, C4=the production-like
    // multi-thread runtime can yield while a synchronous domain port enters its
    // async SQLite adapter. R1 C1+C2+C3+C4 -> awaiting ticket and history
    // recover; R2 !C1 -> fail closed/no implicit Host store; R3 C1+!C2 -> clean
    // independent state; R4 !C4 -> the canonical bridge fails fast rather than
    // deadlocking (proved by awaken-sql-storage's current-thread test).
    // This case proves R1 end to end; the in-memory case below proves the R2/R3
    // non-recovery effect without reintroducing a second Host-owned store path.
    // 1. First "process": execute a Run that awaits on the client tool, then drop the
    //    host — the run's history and awaiting ticket are now only in the store.
    let pending_id = {
        let host = host_over(&dir, "first").await;
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
            !host
                .committed_messages(thread)
                .await
                .expect("committed history remains readable")
                .is_empty(),
            "the Run is committed to durable truth before the restart"
        );
        pending.tool_use_id
    };

    // 2. A brand-new host over the SAME store directory recovers the awaiting run.
    let host = host_over(&dir, "replacement").await;
    assert!(
        host.is_awaiting(thread).await,
        "the rebuilt host recovers the awaiting run from the durable store"
    );
    assert!(
        !host
            .committed_messages(thread)
            .await
            .expect("committed history remains readable")
            .is_empty(),
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

    let history = host
        .committed_messages(thread)
        .await
        .expect("committed history remains readable");
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
    // Cause/effect table: C1 both hosts install the canonical dispatch Session
    // Runtime; C2 each gets an explicitly composed in-memory authority over
    // its own clock; C3 no durable root is shared. R1 C1+C2+C3 -> the first
    // host can await normally, while its replacement starts clean. The testkit
    // composes the sole Ingress MemoryDispatchStore; Host has no implicit store.
    let thread = "ephemeral-1";

    {
        let host = host_with_client_tool(
            awaken_runtime_host_testkit::with_volatile_runtime_authority(
                Arc::into_inner(build_unmounted_host(Arc::new(CustomToolModel), "custom"))
                    .expect("unmounted scenario host has one owner"),
            ),
        );
        host.run(None, thread, vec![user("u1", "hi")])
            .await
            .unwrap();
        assert!(host.is_awaiting(thread).await);
    }

    let host = host_with_client_tool(
        awaken_runtime_host_testkit::with_volatile_runtime_authority(
            Arc::into_inner(build_unmounted_host(Arc::new(CustomToolModel), "custom"))
                .expect("unmounted scenario host has one owner"),
        ),
    );
    assert!(
        !host.is_awaiting(thread).await,
        "an in-memory host starts clean; the awaiting run does not survive"
    );
}
