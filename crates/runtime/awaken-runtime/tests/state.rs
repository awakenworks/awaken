//! Tool-staged state transitions flow through the commit boundary and are
//! replayable from committed truth; an exclusive conflict fails closed (G1/G13).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, Key, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, replay_state};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// Calls a tool once, then ends with text.
struct CallThenEnd {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for CallThenEnd {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "stateful".to_string(),
                arguments: serde_json::json!({}),
            }])
        } else {
            AssistantOutput::text("done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A tool that stages the state commands it was constructed with.
struct StatefulTool(Vec<StateCommand>);

#[async_trait::async_trait]
impl RawTool for StatefulTool {
    fn id(&self) -> &str {
        "stateful"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "noted").with_state(self.0.clone()))
    }
}

struct AllowGate;

#[async_trait::async_trait]
impl ToolGateHook for AllowGate {
    async fn gate(
        &self,
        _ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Allow
    }
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "stateful",
                    "Stage state",
                    serde_json::json!({"type": "object"}),
                )],
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    }
}

#[tokio::test]
async fn tool_staged_state_is_committed_and_replayable() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(StatefulTool(vec![StateCommand::set(
            Scope::Thread,
            MergePolicy::Commutative,
            "counter",
            serde_json::json!({"n": 1}),
        )])))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert_eq!(
        committed
            .state
            .iter()
            .filter(|command| command.key.0 == "counter")
            .count(),
        1,
        "the tool-owned state command is committed alongside Run-scoped ToolBatch checkpoints"
    );

    // A StateChanged event is committed alongside the state event.
    assert!(
        committed
            .events
            .iter()
            .any(|e| e.kind == EventKind::StateChanged)
    );

    // Replay rebuilds the materialized store from committed truth.
    let store = replay_state(&committed);
    assert_eq!(
        store.get(Scope::Thread, &Key("counter".into())),
        Some(&serde_json::json!({"n": 1}))
    );
}

#[tokio::test]
async fn exclusive_conflict_fails_closed_and_commits_no_state() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(StatefulTool(vec![
            StateCommand::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(1),
            ),
            StateCommand::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(2),
            ),
        ])))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::StateConflict)),
        "exclusive conflict fails closed"
    );
    let committed = commit.committed();
    assert!(
        committed
            .state
            .iter()
            .all(|command| command.key.0 != "lock"),
        "the conflicting tool-owned batch is never committed"
    );
    assert!(matches!(
        committed.latest_run.unwrap().state,
        RunState::Ended(EndCause::Error(Failure::StateConflict))
    ));
}
