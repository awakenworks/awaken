//! Tool-produced state side-effects across *multiple* tool calls in one turn.
//!
//! Ported (adapted) from the reference `tool_side_effects.rs`. The reference's
//! `ToolOutput::with_command` + `StateKey`/merge-strategy model maps here onto
//! `ToolOutput::with_state(Vec<StateCommand>)` + `MergePolicy`. Behaviors covered:
//!   * a tool that stages no state commits nothing extra (empty command);
//!   * two tool calls in one turn each staging a `Commutative` write to the same
//!     key both commit and shallow-merge;
//!   * two tool calls each staging an `Exclusive` write to the same key conflict
//!     and fail closed (no partial commit) — the batch validated across calls.
//!
//! Current's `state.rs` already covers the single-tool commit/replay and the
//! single-tool exclusive conflict; these exercise the cross-tool-call path.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, Key, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, replay_state};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
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

/// Emits a single turn with the given tool calls, then ends with text.
struct CallsThenEnd {
    calls: std::sync::Mutex<Option<Vec<ToolCall>>>,
    seen: AtomicUsize,
}

impl CallsThenEnd {
    fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            calls: std::sync::Mutex::new(Some(calls)),
            seen: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for CallsThenEnd {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            let calls = self.calls.lock().unwrap().take().unwrap_or_default();
            AssistantOutput::from_tool_calls(calls)
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

/// A tool with a fixed id that stages the state commands it was built with.
struct FixedTool {
    id: &'static str,
    state: Vec<StateCommand>,
}

#[async_trait::async_trait]
impl RawTool for FixedTool {
    fn id(&self) -> &str {
        self.id
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "noted").with_state(self.state.clone()))
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

fn install(runtime: &Runtime, tool_ids: &[&str]) {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("catalog installs");
    let _ = tool_ids;
}

fn activation(tool_ids: &[&str]) -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    let tool_descriptors = tool_ids
        .iter()
        .map(|id| {
            ToolDescriptor::pinned(
                "test",
                *id,
                "stage state",
                serde_json::json!({"type": "object"}),
            )
        })
        .collect();
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
                tool_descriptors,
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

/// A tool that stages nothing must not add any state to the commit.
#[tokio::test]
async fn tool_with_no_state_commits_nothing_extra() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![ToolCall {
            call_id: "c1".to_string(),
            tool_id: "plain".to_string(),
            arguments: serde_json::json!({}),
        }])))
        .with_tool(Arc::new(FixedTool {
            id: "plain",
            state: vec![],
        }))
        .with_gate(Arc::new(AllowGate));
    install(&runtime, &["plain"]);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["plain"]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .state
            .iter()
            .all(|command| command.key.0 == "runtime.active_tool_batch.v1"),
        "a tool staging no commands must commit only runtime recovery state, got {:?}",
        committed.state
    );
}

/// Two tool calls in one turn, each staging a `Commutative` object write to the
/// same key, both commit and shallow-merge into one value.
#[tokio::test]
async fn parallel_commutative_tool_writes_merge() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "mutate_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "mutate_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(FixedTool {
            id: "mutate_a",
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "acc",
                serde_json::json!({"a": 1}),
            )],
        }))
        .with_tool(Arc::new(FixedTool {
            id: "mutate_b",
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "acc",
                serde_json::json!({"b": 2}),
            )],
        }))
        .with_gate(Arc::new(AllowGate));
    install(&runtime, &["mutate_a", "mutate_b"]);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["mutate_a", "mutate_b"]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert_eq!(
        committed
            .state
            .iter()
            .filter(|command| command.key.0 == "acc")
            .count(),
        2,
        "both tool-owned writes are committed"
    );

    // Replay proves the two commutative writes shallow-merge into one value.
    let store = replay_state(&committed);
    assert_eq!(
        store.get(Scope::Thread, &Key("acc".into())),
        Some(&serde_json::json!({"a": 1, "b": 2})),
        "both commutative writes must be visible after replay"
    );
}

/// Two tool calls in one turn each staging an `Exclusive` write to the same key
/// conflict across the accumulated batch and fail closed — no state is committed.
#[tokio::test]
async fn parallel_exclusive_tool_writes_conflict_fail_closed() {
    let exclusive = |v: i64| {
        vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Exclusive,
            "lock",
            serde_json::json!(v),
        )]
    };
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "lock_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "lock_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(FixedTool {
            id: "lock_a",
            state: exclusive(1),
        }))
        .with_tool(Arc::new(FixedTool {
            id: "lock_b",
            state: exclusive(2),
        }))
        .with_gate(Arc::new(AllowGate));
    install(&runtime, &["lock_a", "lock_b"]);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["lock_a", "lock_b"]), context)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::StateConflict)),
        "two exclusive writes to one key must fail closed"
    );
    let committed = commit.committed();
    assert!(
        committed
            .state
            .iter()
            .all(|command| command.key.0 != "lock"),
        "a conflicting tool-owned batch must never partially commit, got {:?}",
        committed.state
    );
}
