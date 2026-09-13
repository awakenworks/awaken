//! The permission axis end to end (ADR-0030): a `PermissionGate` backed by a
//! `ToolPermissionPolicy` gates every protected tool call. allow runs the tool; deny
//! blocks it (and only permission can grant — visibility/registration do not,
//! G9); ask awaits on a decision ticket that a later allow resumes; every decision
//! is audited as a committed `PermissionDecided` event.

#[path = "permission/recovery_evidence.rs"]
mod recovery_evidence;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_ext_compact::{
    CompactArtifact, CompactBackend, CompactConfig, CompactPlugin, CompactRequest,
};
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::CommittedThreadView;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{
    PermissionBasis, PermissionPolicy, PermissionReasonCode, ToolPermissionPolicy,
    ToolPermissionRuling, ToolPermissionVerdict,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
    ToolPresentation, ToolPresentationOverride,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_store_inmem::MemoryCommitCoordinator;

const FP: &str = "catalog-a";
const SNAP: &str = "snapshot-1";
const TICKET: &str = "perm-call-1";

fn echo_calls(arguments: serde_json::Value, count: usize) -> Vec<ToolCall> {
    (1..=count)
        .map(|index| ToolCall {
            call_id: format!("call-{index}"),
            tool_id: "echo".into(),
            arguments: arguments.clone(),
        })
        .collect()
}

/// Requests the configured tool batch once, then ends with text.
struct ToolThenText {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    tool_calls: Vec<ToolCall>,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.requests.lock().unwrap().push(request);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(self.tool_calls.clone())
        } else {
            AssistantOutput::text("all done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
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

/// Deterministic implementation of the existing Compact labor port. The test
/// counts exact requests but does not invent a cache, store, or planner.
struct CountingCompactBackend {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl CompactBackend for CountingCompactBackend {
    async fn prefetch(&self, _request: CompactRequest) {}

    async fn latest_ready(
        &self,
        _scope: &str,
        _at_most_messages: usize,
    ) -> Option<CompactArtifact> {
        None
    }

    async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(CompactArtifact {
            scope: request.scope,
            key: request.key,
            covered_messages: request.covered_messages,
            summary: "durable compact summary".to_string(),
        })
    }
}

/// A policy that returns one fixed decision — the runtime axis under test, not
/// rule evaluation (that lives in `awaken-ext-permission`).
struct FixedPolicy(ToolPermissionVerdict);
#[async_trait::async_trait]
impl ToolPermissionPolicy for FixedPolicy {
    async fn evaluate(&self, _ctx: &ToolCall) -> ToolPermissionVerdict {
        self.0.clone()
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            delegation_limits: Default::default(),
            model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                ModelBinding::new("p".to_string(), "m".to_string(), "b".to_string()),
            ),
            tool_descriptors: vec![ToolDescriptor::pinned(
                "test",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint: fp,
    }
}

fn runtime(ran: Arc<AtomicUsize>, decision: ToolPermissionVerdict) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
            requests: Arc::new(Mutex::new(Vec::new())),
            tool_calls: echo_calls(serde_json::json!({"text": "ping"}), 1),
        }))
        .with_tool(Arc::new(EchoTool { ran }))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(FixedPolicy(
            decision,
        )))));
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
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
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

/// Test design for tool authorization and execution selection.
/// Decision rules: Allow -> execute once and audit allow; Deny -> do not execute
/// and audit deny; Confirm -> await, then an allowed resume executes once; a
/// run-scoped narrowing cannot be widened. This test covers Allow and the three
/// adjacent cases cover Deny, Confirm, and narrowing respectively.
#[tokio::test]
async fn allow_runs_the_tool_and_audits() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), ToolPermissionVerdict::Allow);
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let state = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
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
        ToolPermissionVerdict::Deny {
            reason: "nope".to_string(),
        },
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let state = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "deny did not run the tool");
    assert_eq!(audited_decisions(&commit), vec!["deny"]);
}

#[tokio::test]
async fn per_run_deny_all_cannot_be_widened_by_an_allowing_platform_policy() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), ToolPermissionVerdict::Allow);
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let state = runtime
        .execute(activation().without_tools(), context(&commit))
        .await
        .expect("the restricted Run completes without invoking the tool");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(audited_decisions(&commit), vec!["deny"]);
}

#[tokio::test]
async fn ask_awaits_then_a_resumed_allow_runs_the_tool() {
    // Test design — Causes: permission gate returns RequireConfirmation and an
    // exact correlated allow later resumes it. Effects: execution first awaits
    // with zero tool calls, then commits approval and runs once. Constraints/
    // invariants: approval and the delivery-operation receipt precede the effect,
    // are committed at the same boundary, and must match the durable ticket.
    // Decision rule P1: ask=>Awaiting/zero; exact allow with operation O1
    // =>approved+ResumeApplied(O1)/one/NaturalEnd. The sibling Host decision
    // table covers response-loss retry and changed-operation conflict.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        ran.clone(),
        ToolPermissionVerdict::RequireConfirmation {
            correlation_id: TICKET.to_string(),
        },
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Ask awaits the run on a decision ticket; the tool has not run.
    let state = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("awaits");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(audited_decisions(&commit), vec!["ask"]);
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a decision ticket is committed");
    assert_eq!(ticket.correlation_id, TICKET);

    // The operator's allow decision, correlated to the ticket, resumes the call.
    let resume = ResumeCommand {
        operation_id: Some("session-thread-reply-operation-1".to_string()),
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
        result: ResumeResult::allow(),
        context_messages: Vec::new(),
        now_ms: 0,
    };
    let state = runtime
        .resume(resume, commit.as_ref(), context(&commit))
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the approved tool ran once");
    assert_eq!(
        audited_decisions(&commit),
        vec!["ask", "approved"],
        "the resume decision is committed before tool execution"
    );
    let committed = commit.committed();
    let receipts = committed
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ResumeApplied)
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1, "one delivery operation has one receipt");
    assert_eq!(
        receipts[0].payload["operation_id"],
        "session-thread-reply-operation-1"
    );
    assert_eq!(receipts[0].payload["correlation_id"], TICKET);
}

#[tokio::test]
async fn compact_context_survives_permission_await_and_resume_without_refolding() {
    // Cause/effect graph: C1 a long parent context crosses the hard Compact
    // threshold; C2 the first model response requests a confirmation-gated tool;
    // C3 an exact allow resumes the same durable Run. Effects: E1 the first leg
    // computes and commits one summary/window before Awaiting; E2 the resumed
    // request reuses that request-only summary; E3 the compactor runs exactly
    // once; E4 the approved tool runs once and the Run ends naturally.
    //
    // | Rule | hard fold | approval | resume | Effects |
    // | R1   | yes       | pending  | no     | E1      |
    // | R2   | committed | allowed  | yes    | E2+E3+E4|
    //
    // Constraints/invariants: ContextMessages/ContextWindow remain the only
    // context authority; Awaiting and approval add no compact cache or replay
    // path; request-only summary text is never appended to the transcript.
    let ran = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let compact_backend = Arc::new(CountingCompactBackend {
        calls: AtomicUsize::new(0),
    });
    let compact_config = CompactConfig {
        keep_last: 0,
        max_tokens: Some(1),
        ..Default::default()
    };
    let compact = Arc::new(
        CompactPlugin::new(compact_config.clone())
            .with_backend("thread-1", compact_backend.clone()),
    );

    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
            requests: requests.clone(),
            tool_calls: echo_calls(serde_json::json!({"text": "ping"}), 1),
        }))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(FixedPolicy(
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: TICKET.to_string(),
            },
        )))))
        .with_plugin(compact);

    let mut activation = activation();
    activation.input[0] = Message {
        id: MessageId("m1".to_string()),
        role: Role::User,
        content: vec![ContentBlock::text(
            "long parent context that crosses the compact token threshold",
        )],
    };
    activation
        .snapshot
        .resolved_spec
        .plugin_ids
        .push("compact".to_string());
    activation.snapshot.resolved_spec.plugin_config.insert(
        "compact".to_string(),
        serde_json::to_value(compact_config).expect("compact config serializes"),
    );
    runtime.register_snapshot(activation.snapshot.clone());

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let first = runtime
        .execute(activation, context(&commit))
        .await
        .expect("R1 awaits permission after compacting");
    assert_eq!(first, RunState::Awaiting, "R1/E1");
    assert_eq!(compact_backend.calls.load(Ordering::SeqCst), 1, "R1/E1");

    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("R1 commits the permission ticket");
    let resumed = runtime
        .resume(
            ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0),
            commit.as_ref(),
            context(&commit),
        )
        .await
        .expect("R2 resumes from committed context state");
    assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd), "R2/E4");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "R2/E4");
    assert_eq!(
        compact_backend.calls.load(Ordering::SeqCst),
        1,
        "R2/E3: resume must not recompute the compact Agent"
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one request before and after approval");
    for (index, request) in requests.iter().enumerate() {
        assert!(
            request.messages.iter().any(|message| {
                message.role == Role::System
                    && message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("durable compact summary")))
            }),
            "R{}/E2: request reuses committed summary: {:?}",
            index + 1,
            request.messages
        );
    }
    assert!(
        commit
            .committed_messages(&ThreadId("thread-1".to_string()))
            .iter()
            .all(|message| !message.text_content().contains("durable compact summary")),
        "request-only compact context must not become transcript truth"
    );
}

/// A model that calls a tool by its MODEL-FACING ALIAS, not the canonical id.
struct CallsAlias {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for CallsAlias {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "safe_echo".to_string(), // the ALIAS, not "echo"
                arguments: serde_json::json!({"text": "ping"}),
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

/// A policy that records every tool id the gate hands it and denies ONLY the
/// canonical `echo`. If the reverse-map runs before the gate, it sees `echo` and
/// denies; if the alias leaked, it would see `safe_echo`, not match, and allow.
struct RecordingDenyCanonical {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ToolPermissionPolicy for RecordingDenyCanonical {
    async fn evaluate(&self, ctx: &ToolCall) -> ToolPermissionVerdict {
        self.seen.lock().unwrap().push(ctx.tool_id.clone());
        if ctx.tool_id == "echo" {
            ToolPermissionVerdict::Deny {
                reason: "canonical echo denied".to_string(),
            }
        } else {
            ToolPermissionVerdict::Allow
        }
    }
}

fn snapshot_aliased() -> ExecutableAgentSnapshot {
    let mut snap = snapshot();
    // Present the canonical `echo` to the model as `safe_echo`.
    snap.resolved_spec.tool_presentation = ToolPresentation::from_overrides([(
        "echo".to_string(),
        ToolPresentationOverride {
            alias: Some("safe_echo".to_string()),
            description: None,
            exposure: None,
        },
    )]);
    snap
}

#[tokio::test]
async fn a_tool_alias_reverse_maps_to_canonical_before_the_permission_gate() {
    // ADR-0053 / audit #85: the presentation alias must be reversed to the canonical
    // id at the single ingress BEFORE the permission gate — so a policy keyed on the
    // canonical id governs the aliased call and the alias never leaks to the gate.
    let ran = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsAlias {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() })) // canonical id "echo"
        .with_gate(Arc::new(PermissionGate::new(Arc::new(
            RecordingDenyCanonical { seen: seen.clone() },
        ))));
    runtime.register_snapshot(snapshot_aliased());

    let mut act = activation();
    act.snapshot = snapshot_aliased();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let state = runtime.execute(act, context(&commit)).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // The gate received the CANONICAL id and denied it, so the aliased call never ran.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|id| id == "echo"),
        "the gate saw the canonical id: {seen:?}"
    );
    assert!(
        !seen.iter().any(|id| id == "safe_echo"),
        "the alias must never reach the gate: {seen:?}"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the canonical deny rule blocked the aliased call"
    );
    assert_eq!(audited_decisions(&commit), vec!["deny"]);
}

/// Observe the actual effect boundary, including the truth visible to a tool.
/// The same probe is bound in the initial Native attempt and the model-free
/// continuation, so the test cannot confuse a generated prompt with invocation.
struct PermissionEffectProbe {
    commit: Arc<MemoryCommitCoordinator>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    fail: bool,
}

#[async_trait::async_trait]
impl RawTool for PermissionEffectProbe {
    fn id(&self) -> &str {
        "echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        use awaken_agent_contract::agent::state::{StateKey, Store};
        use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolCallPhase};
        assert_eq!(
            self.commit.run_state(&RunId("run-1".into())),
            Some(RunState::Running)
        );
        assert!(
            self.commit
                .resume_ticket_for(&RunId("run-1".into()))
                .is_none()
        );
        let batch = ActiveToolBatch::load(&Store::rebuild(
            &self.commit.committed_state(&ThreadId("thread-1".into())),
        ))
        .unwrap()
        .unwrap();
        assert!(matches!(
            batch
                .calls()
                .iter()
                .find(|entry| entry.call.call_id == call.call_id)
                .unwrap()
                .phase,
            ToolCallPhase::Executing { .. }
        ));
        assert_eq!(audited_decisions(&self.commit).last().unwrap(), "approved");
        assert!(
            self.commit
                .committed()
                .events
                .iter()
                .any(|event| event.kind == EventKind::ResumeApplied
                    && event.payload["operation_id"]
                        == format!("tool-handoff-operation-{}", call.call_id))
        );
        self.calls.lock().unwrap().push(call.clone());
        Ok(if self.fail {
            ToolOutput::error(call.call_id, "effect rejected by tool")
        } else {
            ToolOutput::ok(call.call_id, "exact request executed")
        })
    }
}

struct AskByCall;

#[async_trait::async_trait]
impl ToolPermissionPolicy for AskByCall {
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionVerdict {
        ToolPermissionVerdict::RequireConfirmation {
            correlation_id: format!("permission-{}", call.call_id),
        }
    }
}

struct PermissionHandoffFixture {
    runtime: Runtime,
    commit: Arc<MemoryCommitCoordinator>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    command: ResumeCommand,
    arguments: serde_json::Value,
}

impl PermissionHandoffFixture {
    async fn awaiting(fail_tool: bool) -> Self {
        Self::awaiting_calls(fail_tool, 1).await
    }

    async fn awaiting_calls(fail_tool: bool, count: usize) -> Self {
        // Deliberately contains quotes, backslashes, line breaks and 2/3/4-byte
        // UTF-8 characters, at a size above the real 16k Write failure. This is
        // committed data; no model in the resumed Runtime can regenerate it.
        let arguments = serde_json::json!({"text": "é中文🙂\\\"\n".repeat(3000)});
        assert!(arguments["text"].as_str().unwrap().chars().count() > 16_073);
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tool = Arc::new(PermissionEffectProbe {
            commit: commit.clone(),
            calls: calls.clone(),
            fail: fail_tool,
        });
        let initial = Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
                requests: Arc::new(Mutex::new(Vec::new())),
                tool_calls: echo_calls(arguments.clone(), count),
            }))
            .with_tool(tool.clone())
            .with_gate(Arc::new(PermissionGate::new(Arc::new(AskByCall))));
        assert_eq!(
            initial
                .execute(activation(), context(&commit))
                .await
                .unwrap(),
            RunState::Awaiting
        );
        assert!(calls.lock().unwrap().is_empty());
        let ticket = commit.resume_ticket_for(&RunId("run-1".into())).unwrap();
        let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0)
            .with_operation_id("tool-handoff-operation-call-1");
        // A rebuilt host has no Native inference provider. It registers the
        // publication and binds the same owned tool, not an external MCP client.
        let runtime = Runtime::new()
            .with_tool(tool)
            .with_gate(Arc::new(PermissionGate::new(Arc::new(AskByCall))));
        runtime.register_snapshot(snapshot());
        Self {
            runtime,
            commit,
            calls,
            command,
            arguments,
        }
    }

    fn results(&self) -> Vec<(String, bool)> {
        self.commit
            .committed()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.clone(), *is_error)),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn tool_batch_handoff_commits_exact_results_without_native_inference() {
    // Cause/effect graph: C1 exact allow/deny; C2 tool success/error; C3 no
    // Native model. Constraints: only allow enters the effect; approval +
    // ResumeApplied + Executing must already be durable at entry. Results and
    // context commit before returning Running, never a synthetic terminal fact.
    // | Rule | decision | tool outcome | calls | result | state |
    // | H1   | allow    | success      | 1     | ok     | Running |
    // | H2   | deny     | irrelevant   | 0     | error  | Running |
    // | H3   | allow    | error        | 1     | error  | Running |
    // H4: after any row, replaying the consumed command fails with no changes;
    // a reclaimed Native attempt sees the receipt and never re-enters the tool.
    for (allow, fail_tool) in [(true, false), (false, false), (true, true)] {
        let mut fixture = PermissionHandoffFixture::awaiting(fail_tool).await;
        if !allow {
            fixture.command.result = ResumeResult::deny(Some("operator denied".into()));
        }
        fixture.command.context_messages = vec![Message::text(
            MessageId("accepted-handoff-context".into()),
            Role::System,
            "accepted context",
        )];
        let state = fixture
            .runtime
            .resume_tool_batch(
                fixture.command.clone(),
                fixture.commit.as_ref(),
                context(&fixture.commit),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Running);
        let calls = fixture.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), usize::from(allow));
        if allow {
            assert_eq!(calls[0].call_id, "call-1");
            assert_eq!(calls[0].tool_id, "echo");
            assert_eq!(calls[0].arguments, fixture.arguments);
        }
        assert_eq!(
            fixture.results(),
            vec![("call-1".into(), !allow || fail_tool)]
        );
        assert_eq!(
            fixture
                .commit
                .committed()
                .messages
                .iter()
                .filter(|message| message.id.0 == "accepted-handoff-context")
                .count(),
            1
        );
        assert!(
            fixture
                .commit
                .resume_ticket_for(&fixture.command.run_id)
                .is_none()
        );
        let commits = fixture.commit.commit_count();
        assert!(
            fixture
                .runtime
                .resume_tool_batch(
                    fixture.command.clone(),
                    fixture.commit.as_ref(),
                    context(&fixture.commit),
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.commit.commit_count(), commits);

        // H4a: a finalized batch may be handed back after a lost response;
        // the model-free reclaim reads committed truth and adds no commit/effect.
        let mut recovery = activation();
        recovery.input.clear();
        assert_eq!(
            fixture
                .runtime
                .recover_tool_batch(
                    recovery,
                    context(&fixture.commit).with_reader(fixture.commit.clone()),
                )
                .await
                .unwrap(),
            RunState::Running
        );
        assert_eq!(fixture.commit.commit_count(), commits);
        assert_eq!(fixture.calls.lock().unwrap().len(), usize::from(allow));

        let requests = Arc::new(Mutex::new(Vec::new()));
        let next = fixture.runtime.with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(1),
            requests: requests.clone(),
            tool_calls: echo_calls(fixture.arguments.clone(), 1),
        }));
        let mut reclaim = activation();
        reclaim.input.clear();
        assert_eq!(
            next.execute(
                reclaim,
                context(&fixture.commit).with_reader(fixture.commit.clone())
            )
            .await
            .unwrap(),
            RunState::Ended(EndCause::NaturalEnd)
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), usize::from(allow));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let model_results: Vec<_> = requests[0]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
                .collect();
            assert_eq!(
                model_results.len(),
                1,
                "the next model sees committed tool truth"
            );
            assert!(
                matches!(model_results[0], ContentBlock::ToolResult { tool_use_id, is_error, .. }
                if tool_use_id == "call-1" && *is_error == (!allow || fail_tool))
            );
        }
        let results: Vec<_> = fixture
            .commit
            .committed()
            .messages
            .into_iter()
            .flat_map(|message| message.content)
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .collect();
        assert_eq!(
            results.len(),
            1,
            "reclaim must not republish the tool result"
        );
    }
}

#[tokio::test]
async fn tool_batch_handoff_rejects_missing_durability_and_invalid_identity() {
    // H5 missing coordinator; H6 changed correlation. Both are preconditions:
    // reject before any mutation/effect and keep the original ticket resumable.
    for missing_commit in [true, false] {
        let mut fixture = PermissionHandoffFixture::awaiting(false).await;
        let count = fixture.commit.commit_count();
        let context = if missing_commit {
            RuntimeRunContext::new()
        } else {
            fixture.command.correlation_id.push_str("-changed");
            context(&fixture.commit)
        };
        assert!(
            fixture
                .runtime
                .resume_tool_batch(fixture.command.clone(), fixture.commit.as_ref(), context)
                .await
                .is_err()
        );
        assert_eq!(fixture.commit.commit_count(), count);
        assert!(fixture.calls.lock().unwrap().is_empty());
        assert_eq!(
            fixture.commit.run_state(&RunId("run-1".into())),
            Some(RunState::Awaiting)
        );
        assert!(fixture.results().is_empty());
    }
}

struct RejectHandoffCommit {
    inner: Arc<MemoryCommitCoordinator>,
    calls: AtomicUsize,
    reject_at: usize,
}

#[async_trait::async_trait]
impl awaken_agent_contract::thread::commit::coordinator::Coordinator for RejectHandoffCommit {
    async fn commit(
        &self,
        commit: awaken_agent_contract::thread::commit::staged::ThreadCommit,
    ) -> Result<
        awaken_agent_contract::thread::commit::staged::CommitRecord,
        awaken_agent_contract::thread::commit::coordinator::Error,
    > {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.reject_at {
            return Err(
                awaken_agent_contract::thread::commit::coordinator::Error::Rejected(
                    "injected handoff commit failure".into(),
                ),
            );
        }
        self.inner.commit(commit).await
    }
}

#[tokio::test]
async fn tool_batch_handoff_commit_failure_preserves_the_execution_boundary() {
    // Causes: H7 reject the atomic approval+ResumeApplied commit; H8 accept that
    // commit then reject the later result commit. Effects: H7 stays Awaiting
    // with zero receipt/effect/result; H8 exposes Running+one exact receipt and
    // one effect, but no invented result. Thus a commit error cannot expose one
    // half of ticket consumption + receipt. On H8 reclaim the pinned NeverReplay
    // policy produces an indeterminate result without repeating the effect.
    for reject_at in [0, 1] {
        let fixture = PermissionHandoffFixture::awaiting(false).await;
        let rejecting = Arc::new(RejectHandoffCommit {
            inner: fixture.commit.clone(),
            calls: AtomicUsize::new(0),
            reject_at,
        });
        assert!(
            fixture
                .runtime
                .resume_tool_batch(
                    fixture.command.clone(),
                    fixture.commit.as_ref(),
                    RuntimeRunContext::new().with_commit(rejecting),
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), reject_at);
        assert!(fixture.results().is_empty());
        assert_eq!(
            fixture
                .commit
                .resume_ticket_for(&fixture.command.run_id)
                .is_some(),
            reject_at == 0
        );
        assert_eq!(
            fixture.commit.run_state(&fixture.command.run_id),
            Some(if reject_at == 0 {
                RunState::Awaiting
            } else {
                RunState::Running
            })
        );
        let resume_receipts = fixture
            .commit
            .committed()
            .events
            .into_iter()
            .filter(|event| event.kind == EventKind::ResumeApplied)
            .collect::<Vec<_>>();
        assert_eq!(resume_receipts.len(), reject_at, "H7/H8 receipt frame");
        if let Some(receipt) = resume_receipts.first() {
            assert_eq!(
                receipt.payload["operation_id"],
                fixture.command.operation_id.as_deref().unwrap()
            );
            assert_eq!(
                receipt.payload["correlation_id"],
                fixture.command.correlation_id
            );
        }
        if reject_at == 1 {
            // H8a: external-loop reclaim has no Native model. It must publish
            // the NeverReplay indeterminate receipt before returning Running.
            let mut recovery = activation();
            recovery.input.clear();
            assert_eq!(
                fixture
                    .runtime
                    .recover_tool_batch(
                        recovery,
                        context(&fixture.commit).with_reader(fixture.commit.clone()),
                    )
                    .await
                    .unwrap(),
                RunState::Running
            );
            assert_eq!(fixture.calls.lock().unwrap().len(), 1);
            assert_eq!(fixture.results(), vec![("call-1".into(), true)]);
            let next = fixture.runtime.with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(1),
                requests: Arc::new(Mutex::new(Vec::new())),
                tool_calls: echo_calls(fixture.arguments.clone(), 1),
            }));
            let mut reclaim = activation();
            reclaim.input.clear();
            assert_eq!(
                next.execute(
                    reclaim,
                    context(&fixture.commit).with_reader(fixture.commit.clone())
                )
                .await
                .unwrap(),
                RunState::Ended(EndCause::NaturalEnd)
            );
            assert_eq!(fixture.calls.lock().unwrap().len(), 1);
            let results: Vec<_> = fixture
                .commit
                .committed()
                .messages
                .into_iter()
                .flat_map(|message| message.content)
                .filter(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
                .collect();
            assert_eq!(results.len(), 1);
            assert!(
                serde_json::to_string(&results)
                    .unwrap()
                    .contains("indeterminate")
            );
        }
    }
}

#[tokio::test]
async fn tool_batch_handoff_preserves_the_remaining_calls_and_publication_barrier() {
    // H9: two ordered calls, both Ask. Approve first => execute only first,
    // commit its private batch result and a ticket for second, return Awaiting
    // without publishing a partial result set or invoking a Native model.
    // H10/H11: allow/deny second => execute once/zero, publish both results in
    // original order and return Running. First result is never re-executed.
    // A stale first approval cannot consume the second call's distinct ticket.
    for allow_second in [true, false] {
        let fixture = PermissionHandoffFixture::awaiting_calls(false, 2).await;
        assert_eq!(
            fixture
                .runtime
                .resume_tool_batch(
                    fixture.command.clone(),
                    fixture.commit.as_ref(),
                    context(&fixture.commit),
                )
                .await
                .unwrap(),
            RunState::Awaiting
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        assert!(
            fixture.results().is_empty(),
            "partial batch stays behind publication barrier"
        );
        let ticket = fixture
            .commit
            .resume_ticket_for(&fixture.command.run_id)
            .unwrap();
        assert_eq!(ticket.call_id(), Some("call-2"));
        assert!(
            fixture
                .runtime
                .resume_tool_batch(
                    fixture.command.clone(),
                    fixture.commit.as_ref(),
                    context(&fixture.commit),
                )
                .await
                .is_err()
        );
        let decision = if allow_second {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(Some("second request denied".into()))
        };
        let command = ResumeCommand::from_ticket(&ticket, decision, 0)
            .with_operation_id("tool-handoff-operation-call-2");
        assert_eq!(
            fixture
                .runtime
                .resume_tool_batch(command, fixture.commit.as_ref(), context(&fixture.commit),)
                .await
                .unwrap(),
            RunState::Running
        );
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1 + usize::from(allow_second));
        for (index, call) in calls.iter().enumerate() {
            assert_eq!(call.call_id, format!("call-{}", index + 1));
            assert_eq!(call.arguments, fixture.arguments);
        }
        assert_eq!(
            fixture.results(),
            vec![("call-1".into(), false), ("call-2".into(), !allow_second)]
        );
    }
}

/// A policy that states the authored basis of its verdict, as the rule-based
/// extension does. The runtime axis under test is that the basis a policy
/// states reaches the committed audit fact untouched — not how the extension
/// arrives at it (that is `awaken-ext-permission`'s own decision table).
struct BasisPolicy(ToolPermissionRuling);
#[async_trait::async_trait]
impl ToolPermissionPolicy for BasisPolicy {
    async fn evaluate(&self, _ctx: &ToolCall) -> ToolPermissionVerdict {
        self.0.verdict.clone()
    }

    async fn evaluate_with_basis(&self, _ctx: &ToolCall) -> ToolPermissionRuling {
        self.0.clone()
    }
}

fn audited_rows(commit: &MemoryCommitCoordinator) -> Vec<(String, Option<String>, Option<String>)> {
    commit
        .committed()
        .events
        .iter()
        .filter(|e| e.kind == EventKind::PermissionDecided)
        .map(|e| {
            (
                e.payload["decision"].as_str().unwrap_or("").to_string(),
                e.payload
                    .get("policy")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                e.payload
                    .get("reason_code")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_gate_audits_the_policy_and_reason_the_deciding_policy_stated() {
    // Cause/effect graph: C1 the authored policy fixed the answer
    // (always_allow / always_ask); C2 the policy was `auto` and the evaluator
    // allowed, refused, or reached no conclusion; C3 the policy stated no basis
    // (a fixed host policy outside the authored vocabulary). Effects: E1 the
    // committed `PermissionDecided` row carries the decision label the gate
    // outcome maps to; E2 `policy` is the stated policy; E3 `reason_code` is
    // present exactly for an `auto` refusal (`judged_high_risk`) or an `auto`
    // ask (`indeterminate`); E4 with no stated basis neither key is written.
    //
    // | Rule | verdict | stated basis | decision | policy | reason_code |
    // |---|---|---|---|---|---|
    // | G1 | Allow | always_allow | allow | always_allow | - |
    // | G2 | RequireConfirmation | always_ask | ask | always_ask | - |
    // | G3 | Allow | auto | allow | auto | - |
    // | G4 | Deny | auto/judged_high_risk | deny | auto | judged_high_risk |
    // | G5 | RequireConfirmation | auto/indeterminate | ask | auto | indeterminate |
    // | G6 | Deny | none | deny | - | - |
    let ask = || ToolPermissionVerdict::RequireConfirmation {
        correlation_id: TICKET.to_string(),
    };
    let deny = || ToolPermissionVerdict::Deny {
        reason: "no".to_string(),
    };
    /// `(decision, policy, reason_code)` as the committed payload renders them.
    type AuditedRow<'a> = (&'a str, Option<&'a str>, Option<&'a str>);
    let rows: [(&str, ToolPermissionRuling, AuditedRow<'_>); 6] = [
        (
            "G1",
            ToolPermissionRuling {
                verdict: ToolPermissionVerdict::Allow,
                basis: Some(PermissionBasis::fixed(PermissionPolicy::AlwaysAllow)),
            },
            ("allow", Some("always_allow"), None),
        ),
        (
            "G2",
            ToolPermissionRuling {
                verdict: ask(),
                basis: Some(PermissionBasis::fixed(PermissionPolicy::AlwaysAsk)),
            },
            ("ask", Some("always_ask"), None),
        ),
        (
            "G3",
            ToolPermissionRuling {
                verdict: ToolPermissionVerdict::Allow,
                basis: Some(PermissionBasis::auto(None)),
            },
            ("allow", Some("auto"), None),
        ),
        (
            "G4",
            ToolPermissionRuling {
                verdict: deny(),
                basis: Some(PermissionBasis::auto(Some(
                    PermissionReasonCode::JudgedHighRisk,
                ))),
            },
            ("deny", Some("auto"), Some("judged_high_risk")),
        ),
        (
            "G5",
            ToolPermissionRuling {
                verdict: ask(),
                basis: Some(PermissionBasis::auto(Some(
                    PermissionReasonCode::Indeterminate,
                ))),
            },
            ("ask", Some("auto"), Some("indeterminate")),
        ),
        (
            "G6",
            ToolPermissionRuling::without_basis(deny()),
            ("deny", None, None),
        ),
    ];
    for (rule, ruling, (decision, policy, reason_code)) in rows {
        let ran = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
                requests: Arc::new(Mutex::new(Vec::new())),
                tool_calls: echo_calls(serde_json::json!({"text": "ping"}), 1),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(PermissionGate::new(Arc::new(BasisPolicy(ruling)))));
        runtime.register_snapshot(snapshot());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        runtime
            .execute(activation(), context(&commit))
            .await
            .expect("the gate decides and the run commits");
        assert_eq!(
            audited_rows(&commit),
            vec![(
                decision.to_string(),
                policy.map(str::to_string),
                reason_code.map(str::to_string)
            )],
            "{rule}"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            usize::from(decision == "allow"),
            "{rule}: only an allow runs the tool"
        );
    }
}

#[tokio::test]
async fn a_per_run_deny_all_narrowing_audits_no_policy_even_over_a_stating_gate() {
    // The narrowing decides before any authored policy is consulted, so the
    // row it writes names none — attributing the denial to the authored
    // `always_allow` would claim the author refused a call they allowed.
    //
    // | Rule | narrowing | host policy basis | decision | policy |
    // |---|---|---|---|---|
    // | N1 | DenyAll | always_allow | deny | - |
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
            requests: Arc::new(Mutex::new(Vec::new())),
            tool_calls: echo_calls(serde_json::json!({"text": "ping"}), 1),
        }))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(BasisPolicy(
            ToolPermissionRuling {
                verdict: ToolPermissionVerdict::Allow,
                basis: Some(PermissionBasis::fixed(PermissionPolicy::AlwaysAllow)),
            },
        )))));
    runtime.register_snapshot(snapshot());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    runtime
        .execute(activation().without_tools(), context(&commit))
        .await
        .expect("the restricted Run completes");
    assert_eq!(
        audited_rows(&commit),
        vec![("deny".to_string(), None, None)],
        "N1"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "N1");
}
