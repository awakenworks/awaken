//! Resume fails closed on the two structural faults the happy-path resume tests
//! never provoke (G5/G28/G30): a ticket whose snapshot is no longer registered
//! (RR3), and a plugin set that violates its capability bound on the resume path
//! (RR5) — the same fail-closed altitude as the initial run, reached via `resume`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::Store;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, PhaseContext, PhaseHook, PhaseHookPoint, Plugin,
    PluginManifest,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";
const TICKET_ID: &str = "ticket-1";

/// Calls `echo` once (which the suspend gate awaits), then would end with text.
struct ToolThenText {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
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
        Ok(ToolOutput::ok(call.call_id, "echoed"))
    }
}

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: TICKET_ID.to_string(),
        }
    }
}

/// A no-op state hook — the out-of-bound contribution the rogue plugin makes.
struct NoopHook;
#[async_trait::async_trait]
impl PhaseHook for NoopHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::StepStart
    }
    async fn on_phase(
        &self,
        _ctx: &PhaseContext,
        _conversation: &[Message],
        _state: &Store,
    ) -> HookReaction {
        HookReaction::default()
    }
}

/// A well-behaved plugin `p`: declares nothing and contributes nothing, so the
/// initial run resolves its env cleanly and reaches the await.
struct NoopPlugin;
impl Plugin for NoopPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "p".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound::default(),
        }
    }
    fn resolve(&self) -> Contributions {
        Contributions::new("p")
    }
}

/// A rogue plugin under the same id `p`: contributes a state hook it never declared
/// in its (empty) bound, so `resolve_plugin_env` fails closed (G30).
struct RoguePlugin;
impl Plugin for RoguePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "p".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound::default(), // declares no state hook
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("p");
        c.phase_hooks.push(Arc::new(NoopHook)); // StepStart, outside the empty bound
        c
    }
}

fn snapshot(plugin_ids: Vec<String>) -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
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
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids,
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn activation(plugin_ids: Vec<String>) -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(plugin_ids),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    }
}

fn resume_command() -> ResumeCommand {
    ResumeCommand {
        correlation_id: TICKET_ID.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result: ResumeResult::Decision {
            allow: false,
            note: None,
        },
        now_ms: 0,
    }
}

/// Await a run on a tool-permission ticket, returning the commit coordinator that
/// holds the committed ticket (the reader a resume validates against).
async fn begin_awaiting_run(
    runtime: &Runtime,
    plugin_ids: Vec<String>,
) -> Arc<MemoryCommitCoordinator> {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation(plugin_ids), context)
        .await
        .expect("initial run awaits");
    assert_eq!(
        state,
        RunState::Awaiting,
        "the run awaiting on a permission ticket"
    );
    commit
}

#[tokio::test]
async fn resume_against_an_unregistered_snapshot_fails_closed() {
    // RR3: a run awaits with a committed ticket; a resume whose runtime no longer has
    // that snapshot registered validates the ticket but cannot resolve the snapshot,
    // so it fails closed instead of guessing.
    let awaiting = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(SuspendGate));
    awaiting.register_snapshot(snapshot(Vec::new()));
    let commit = begin_awaiting_run(&awaiting, Vec::new()).await;

    // A fresh runtime that never registered the snapshot.
    let bare = Runtime::new();
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let err = bare
        .resume(resume_command(), commit.as_ref(), context)
        .await
        .expect_err("an unresolvable snapshot fails the resume closed");
    assert!(
        err.to_string().contains("snapshot"),
        "expected a snapshot-resolution error, got {err}"
    );
    // The run stays awaiting: the ticket is untouched.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_some()
    );
}

#[tokio::test]
async fn resume_with_an_out_of_bound_plugin_fails_capability_bound() {
    // RR5: the resume path re-resolves the plugin env; a plugin that now violates
    // its capability bound fails the resumed run closed with CapabilityBound — the
    // same fail-closed altitude as the initial run (G30), reached via resume.
    let awaiting = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(SuspendGate))
        .with_plugin(Arc::new(NoopPlugin));
    awaiting.register_snapshot(snapshot(vec!["p".to_string()]));
    let commit = begin_awaiting_run(&awaiting, vec!["p".to_string()]).await;

    // The resuming runtime has the same snapshot but a rogue plugin under id `p`.
    let rogue = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(Arc::new(RoguePlugin));
    rogue.register_snapshot(snapshot(vec!["p".to_string()]));

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = rogue
        .resume(resume_command(), commit.as_ref(), context)
        .await
        .expect("resume completes with a terminal fault");
    assert_eq!(
        state,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
        "an out-of-bound plugin fails the resumed run closed"
    );
}
