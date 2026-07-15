//! Committed-event lifecycle invariants for a gated tool-call run.
//!
//! Adapted from the reference `event_ordering_invariants_hold_across_scenarios`.
//! The reference asserted a live event *sequence*; current commits events in
//! batched `ThreadCommit`s, so the stable, non-brittle invariants to lock in are:
//!   * every committed event carries a strictly-increasing, unique `sequence`;
//!   * a gated tool call is audited (a `PermissionDecided` event is committed);
//!   * the run's committed authority ends in a terminal `Ended` phase, and a
//!     `RunPhaseChanged` event rides the same commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

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
                tool_id: "echo".to_string(),
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

struct EchoTool;

#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "ok"))
    }
}

struct AllowGate;

#[async_trait::async_trait]
impl ToolGateHook for AllowGate {
    async fn gate(
        &self,
        _ctx: &PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Allow
    }
}

fn install(runtime: &Runtime) {
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
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "echo",
                    "echoes",
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
        model_access: Default::default(),
    }
}

#[tokio::test]
async fn gated_tool_run_commits_ordered_audited_terminal_events() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(AllowGate));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();

    // Invariant 1: event sequences are strictly increasing and unique.
    let seqs: Vec<u64> = committed.events.iter().map(|e| e.sequence).collect();
    assert!(!seqs.is_empty(), "a run must commit events");
    for pair in seqs.windows(2) {
        assert!(
            pair[1] > pair[0],
            "event sequences must strictly increase, saw {seqs:?}"
        );
    }
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique.len(), seqs.len(), "event sequences must be unique");

    // Invariant 2: the gated tool call is audited.
    assert!(
        committed
            .events
            .iter()
            .any(|e| e.kind == EventKind::PermissionDecided),
        "a gated tool call must commit a PermissionDecided audit event"
    );

    // Invariant 3: a RunPhaseChanged rides the commit and the stored authority is terminal.
    assert!(
        committed
            .events
            .iter()
            .any(|e| e.kind == EventKind::RunPhaseChanged),
        "the terminal commit must carry a RunPhaseChanged event"
    );
    assert_eq!(
        committed.latest_run.expect("a run fact is committed").phase,
        Phase::Ended(EndCause::NaturalEnd),
        "the committed run authority must be terminal"
    );
}
