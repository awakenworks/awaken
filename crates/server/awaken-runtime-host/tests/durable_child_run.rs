//! Production link for independently scheduled child Runs: a parent invokes the
//! ordinary `agent_run` tool, the child receives its own durable dispatch claim,
//! and the result returns through the parent Run.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_run_ingress::{AnyDispatchStore, Dispatch, MemoryDispatchStore};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult, ToolCall,
};
use awaken_runtime_host::{HostResume, SharedHost, init_shared_dispatch_store};

struct ParentChildModel {
    parent_calls: AtomicUsize,
    child_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ParentChildModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let messages = serde_json::to_string(&request.messages).expect("chat messages serialize");
        let is_child = !messages.contains("\"text\":\"start\"");
        let call = if is_child {
            self.child_calls.fetch_add(1, Ordering::SeqCst)
        } else {
            self.parent_calls.fetch_add(1, Ordering::SeqCst)
        };
        let output = match (is_child, call) {
            (false, 0) => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "delegate-1".into(),
                tool_id: awaken_ext_builtin_tools::AGENT_RUN.into(),
                arguments: serde_json::json!({
                    "agent_id": "researcher",
                    "input": "investigate"
                }),
            }]),
            (true, 0) => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "child-permission".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": "child-note.txt",
                    "content": "approved child work"
                }),
            }]),
            (true, _) => AssistantOutput::text("child result"),
            _ => AssistantOutput::text("parent received child result"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_run_uses_the_durable_scheduler_and_returns_to_its_parent() {
    // Dedicated integration-test process: set deployment before constructing the
    // host and inject its one shared queue.
    unsafe {
        std::env::set_var("AWAKEN_INGRESS", "durable");
    }
    let storage = tempfile::tempdir().expect("storage");
    let memory = Arc::new(MemoryDispatchStore::new());
    init_shared_dispatch_store(Arc::new(AnyDispatchStore::from_dispatch(
        memory.clone() as Arc<dyn Dispatch>
    )));

    let host = Arc::new(
        SharedHost::new(
            Arc::new(ParentChildModel {
                parent_calls: AtomicUsize::new(0),
                child_calls: AtomicUsize::new(0),
            }),
            "stub",
        )
        .with_store_dir(storage.path())
        .with_delegates(HashSet::from(["researcher".to_string()])),
    );
    host.ensure_dispatch_pool();

    let awaiting = host
        .run(
            None,
            "parent-thread",
            vec![Message::text(
                MessageId("user-1".into()),
                Role::User,
                "start",
            )],
        )
        .await
        .expect("parent observes the child's interaction boundary");

    assert!(matches!(awaiting.state, RunState::Awaiting));
    let pending = awaiting
        .pending
        .expect("the parent exposes the child permission request");
    assert_eq!(pending.name, "write");
    assert_eq!(awaiting.delegated_runs.len(), 1);
    assert!(
        awaiting.delegated_runs[0]
            .run_id
            .0
            .starts_with("child-run:"),
        "the awaiting child has a first-class stable Run identity"
    );

    // The user addresses the parent session. The host validates the child's
    // committed ticket and routes this decision through the parent relationship;
    // no public API resumes the child directly.
    let result = host
        .resume(
            "parent-thread",
            &pending.tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("parent routes the permission decision to its child");

    assert!(matches!(result.state, RunState::Ended(_)));
    assert!(
        host.committed_messages("parent-thread")
            .await
            .iter()
            .any(|message| message
                .text_content()
                .contains("parent received child result"))
    );
}
