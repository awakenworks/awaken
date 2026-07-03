//! The permission axis end to end (ADR-0030): a `PermissionGate` backed by a
//! `PermissionPolicy` gates every protected tool call. allow runs the tool; deny
//! blocks it (and only permission can grant — visibility/registration do not,
//! G9); ask parks on a decision ticket that a later allow resumes; every decision
//! is audited as a committed `PermissionDecided` event.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{
    PermissionContext, PermissionDecision, PermissionPolicy,
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

const FP: &str = "catalog-a";
const SNAP: &str = "snapshot-1";
const TICKET: &str = "perm-call-1";

/// Calls `echo` once, then ends with text.
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
                arguments: serde_json::json!({"text": "ping"}),
            }])
        } else {
            AssistantOutput::text("all done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

struct EchoTool {
    ran: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, "echoed: ping"))
    }
}

/// A policy that returns one fixed decision — the runtime axis under test, not
/// rule evaluation (that lives in `awaken-ext-permission`).
struct FixedPolicy(PermissionDecision);
#[async_trait::async_trait]
impl PermissionPolicy for FixedPolicy {
    async fn decide(&self, _ctx: &PermissionContext) -> PermissionDecision {
        self.0.clone()
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "test",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
        },
        fingerprint: fp,
    }
}

fn runtime(ran: Arc<AtomicUsize>, decision: PermissionDecision) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran }))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(FixedPolicy(
            decision,
        )))));
    let fp = CatalogFingerprint(FP.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fp.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
    runtime.register_snapshot(snapshot());
    runtime
}

fn activation() -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        trace: Default::default(),
    }
}

fn context(commit: &Arc<MemoryCommitCoordinator>) -> RuntimeRunContext {
    RuntimeRunContext::new().with_commit(commit.clone())
}

fn audited_decisions(commit: &MemoryCommitCoordinator) -> Vec<String> {
    commit
        .committed()
        .events
        .iter()
        .filter(|e| e.kind == EventKind::PermissionDecided)
        .map(|e| e.payload["decision"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn allow_runs_the_tool_and_audits() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), PermissionDecision::Allow);
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let phase = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "allow ran the tool");
    assert_eq!(audited_decisions(&commit), vec!["allow"]);
}

#[tokio::test]
async fn deny_blocks_the_tool_and_only_permission_grants() {
    // G9: the tool is registered and the model selected it, yet it does not run —
    // only the permission decision can grant execution.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        ran.clone(),
        PermissionDecision::Deny {
            reason: "nope".to_string(),
        },
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let phase = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "deny did not run the tool");
    assert_eq!(audited_decisions(&commit), vec!["deny"]);
}

#[tokio::test]
async fn ask_parks_then_a_resumed_allow_runs_the_tool() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        ran.clone(),
        PermissionDecision::Ask {
            ticket_id: TICKET.to_string(),
        },
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Ask parks the run on a decision ticket; the tool has not run.
    let phase = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("parks");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(audited_decisions(&commit), vec!["ask"]);
    let ticket = commit
        .waiting_for(&RunId("run-1".to_string()))
        .expect("a decision ticket is committed");
    assert_eq!(ticket.correlation_id, TICKET);

    // The operator's allow decision, correlated to the ticket, resumes the call.
    let resume = ResumeCommand {
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: SNAP.to_string(),
        catalog_fingerprint: FP.to_string(),
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms: 0,
    };
    let phase = runtime
        .resume(resume, commit.as_ref(), context(&commit))
        .await
        .expect("resume");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the approved tool ran once");
}
