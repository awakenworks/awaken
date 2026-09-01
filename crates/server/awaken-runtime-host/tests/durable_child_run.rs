//! Production link for independently scheduled child Runs: a parent invokes the
//! ordinary `agent_run` tool, the child receives its own durable dispatch claim,
//! and the result returns through the parent Run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_run_ingress::{AnyDispatchStore, Dispatch, DispatchQueue, MemoryDispatchStore};
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolKind};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_runtime_host::{HostResume, SharedHost};

struct ParentChildModel {
    parent_calls: AtomicUsize,
    child_calls: AtomicUsize,
}

fn test_snapshot(agent_id: &str, delegates: Vec<AgentId>) -> ExecutableAgentSnapshot {
    let mut tools = awaken_runtime_host::authorable_tools();
    if delegates.is_empty() {
        tools.retain(|tool| tool.kind != ToolKind::AgentDelegation);
    }
    ExecutableAgentSnapshot::builder(agent_id)
        .model(ModelBinding::new("default", "stub", "default"))
        .tools(tools)
        .agent_bindings(AgentBindings {
            delegates: delegates
                .into_iter()
                .map(|agent_id| AgentDelegateBinding {
                    agent_id,
                    source_revision: None,
                    recursive_self: false,
                })
                .collect(),
            ..Default::default()
        })
        .build()
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

#[test]
fn child_run_uses_the_durable_scheduler_and_returns_to_its_parent() {
    // Cause/effect design: C0 the child publication explicitly sets
    // write=always_ask; C1 the parent Run awaits delegation; C2 the child Run
    // awaits permission in its own thread; C3 the caller approves the exact child
    // tool through the parent; C4 the parent claim projection contains no child
    // Thread; C5 the parent Delegation ticket transports the typed child
    // Permission without reclassifying it; C6 the same canonical service Runtime
    // used by cloud Workers drives the complete future. Effects: E1 expose the
    // child's `write` boundary once; E2 resume that exact child; E3 settle both
    // durable rows; E4 commit the child result and parent NaturalEnd reply; E5
    // child execution does not abort on Tokio's smaller default stack.
    // Constraints: K1 the Session commit is the sole local parent+child read
    // authority; K2 each claim
    // keeps its own projection; K3 the shared queue is the sole execution fence;
    // K4 the parent validates the relationship while the child ticket owns answer
    // kind and call identity. Decision rules: R1=C0+C1+C2=>E1; R2=C0+C1+C2+C3+C5=>
    // E2+E3+E4; R3=C4=>use K1, never a stale parent projection;
    // R4=C1+C2+C6=>E1+E5 through the one service Runtime owner.
    // The injected shared queue is the typed durable-ingress authority; the
    // builder enables its pool without mutating process-global configuration.
    awaken_service_lifecycle::block_on_service(durable_child_run_scenario());
}

async fn durable_child_run_scenario() {
    let storage = tempfile::tempdir().expect("storage");
    let memory = Arc::new(MemoryDispatchStore::new());
    let dispatch = Arc::new(AnyDispatchStore::from_dispatch(
        memory.clone() as Arc<dyn Dispatch>
    ));

    let assistant = test_snapshot("assistant", vec![AgentId("researcher".into())]);
    let mut researcher = test_snapshot("researcher", Vec::new());
    researcher.resolved_spec.plugin_config.agent.toolsets = vec![
        awaken_runtime_contract::agent_bindings::ToolsetPolicy {
            source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
            default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                enabled: true,
                permission:
                    awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: vec![
                awaken_runtime_contract::agent_bindings::ToolPolicyOverride::new(
                    "write",
                    awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                        enabled: true,
                        permission: awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
                    },
                ),
            ],
        },
    ];
    let host = Arc::new(
        SharedHost::new(
            Arc::new(ParentChildModel {
                parent_calls: AtomicUsize::new(0),
                child_calls: AtomicUsize::new(0),
            }),
            "stub",
        )
        .with_dispatch_store(dispatch)
        .with_store_dir(storage.path())
        .with_agent_publications(Arc::new(
            StaticPublishedAgentSnapshots::try_new([assistant, researcher])
                .expect("valid test Agent publications"),
        )),
    );
    host.ensure_dispatch_pool();

    let awaiting = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        host.run(
            None,
            "parent-thread",
            vec![Message::text(
                MessageId("user-1".into()),
                Role::User,
                "start",
            )],
        ),
    )
    .await
    .expect("child Awaiting boundary does not stall on the parent projection")
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
    let child_run_id = awaiting.delegated_runs[0].run_id.clone();
    let resumed = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        host.resume(
            "parent-thread",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        ),
    )
    .await;
    let result = match resumed {
        Ok(result) => result.expect("parent routes the permission decision to its child"),
        Err(_) => panic!(
            "child resume stalled: rows={:?}, child_pending={}, parent={:?}",
            memory.list_dispatches().await,
            memory.pending_count(&child_run_id),
            host.committed_messages("parent-thread").await,
        ),
    };

    assert!(matches!(
        result.state,
        RunState::Ended(EndCause::NaturalEnd)
    ));
    assert!(
        memory
            .list_dispatches()
            .await
            .expect("durable rows remain observable")
            .is_empty(),
        "the child and parent durable rows reach terminal settlement and leave the live queue"
    );
    assert!(
        host.committed_messages("parent-thread")
            .await
            .expect("committed history remains readable")
            .iter()
            .any(|message| message
                .text_content()
                .contains("parent received child result"))
    );
}
