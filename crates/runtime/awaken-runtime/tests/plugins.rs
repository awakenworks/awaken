// @formal evidence session_execution.one_physical_attempt_per_thread
//! Active plugins contribute behavior only through declared seams: a phase hook
//! stages state via the commit path, and an out-of-bound plugin fails closed
//! (G9/G30). Plugins are resolved once per run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{
    Command as StateCommand, Key, MergePolicy, Scope, Store,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{
    DenyAllTools, GateDecision, GateOutcome, PermissionBasis, PermissionPolicy, ToolGateHook,
};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, HookReaction, IdBound, PhaseContext, PhaseHook,
    PhaseHookPoint, Plugin, PluginManifest,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_store_inmem::{MemoryCommitCoordinator, replay_state};

struct TextLlm;

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A hook that stages the current Run input at BeforeInference.
struct MarkHook;

#[async_trait::async_trait]
impl PhaseHook for MarkHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }
    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        _conversation: &[Message],
        _state: &Store,
    ) -> HookReaction {
        let awaken_runtime_contract::plugin::PhaseKind::BeforeInference { run_input } = &ctx.kind
        else {
            unreachable!("MarkHook is registered only at BeforeInference")
        };
        HookReaction::state(vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "phase",
            serde_json::json!({
                "phase": format!("{:?}@{}", ctx.kind.point(), ctx.step),
                "run_input": run_input.iter().map(Message::text_content).collect::<Vec<_>>(),
            }),
        )])
    }
}

/// A well-behaved plugin: declares the hook point and state key it uses, and
/// counts how many times it is resolved.
struct MarkPlugin {
    resolves: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum PhaseProbeMode {
    Return,
    Pending,
    Panic,
}

#[derive(Clone)]
struct PhaseProbe {
    point: PhaseHookPoint,
    entered: Arc<tokio::sync::Notify>,
    mode: PhaseProbeMode,
}

#[async_trait::async_trait]
impl PhaseHook for PhaseProbe {
    fn point(&self) -> PhaseHookPoint {
        self.point
    }

    async fn on_phase(&self, _: &PhaseContext, _: &[Message], _: &Store) -> HookReaction {
        self.entered.notify_one();
        match self.mode {
            PhaseProbeMode::Return => HookReaction::default(),
            PhaseProbeMode::Pending => std::future::pending().await,
            PhaseProbeMode::Panic => panic!("injected phase failure"),
        }
    }
}

impl Plugin for PhaseProbe {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "phase-probe".into(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                phase_hooks: vec![self.point],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("phase-probe");
        contributions.phase_hooks.push(Arc::new(self.clone()));
        contributions
    }
}

fn phase_probe_runtime(probe: PhaseProbe) -> Runtime {
    Runtime::new()
        .with_llm(Arc::new(ToolThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(CountingEcho {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .with_plugin(Arc::new(probe))
}

#[tokio::test]
async fn every_entered_phase_hook_vetoes_ack_on_drop_or_unwind_but_not_return() {
    // Causes H1 five production phase points plus recovered AfterTool;
    // H2 normal return / pending Future
    // drop / panic. Effects E1 normal return preserves the prior latch; E2 an
    // entered unreturned participant vetoes ACK. Cross H1 x H2 (18 cases),
    // through the actual Native loop, not direct invocation of the observer.
    // Await the entry barrier before aborting; join proves the Future was dropped.
    // This is process-local veto evidence, not remote termination or hard-crash
    // recovery. The complete durable participant inventory remains separate.
    for (point, recovered) in [
        (PhaseHookPoint::StepStart, false),
        (PhaseHookPoint::BeforeInference, false),
        (PhaseHookPoint::AfterInference, false),
        (PhaseHookPoint::AfterTool, false),
        (PhaseHookPoint::StepEnd, false),
        (PhaseHookPoint::AfterTool, true),
    ] {
        for mode in [
            PhaseProbeMode::Return,
            PhaseProbeMode::Pending,
            PhaseProbeMode::Panic,
        ] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let mut runtime = phase_probe_runtime(PhaseProbe {
                point,
                entered: entered.clone(),
                mode,
            });
            let activation = activation(vec!["phase-probe".into()]);
            let mut context = RuntimeRunContext::new();
            if recovered {
                use awaken_agent_contract::agent::state::StateKey;
                use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatch};
                use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};
                // A committed Requested call needs no replay permission. A
                // text-only model cannot accidentally supply a fresh AfterTool
                // witness if recovery is skipped. This tests the Runtime fold,
                // not permission to bypass an occupied predecessor Dispatch.
                runtime = runtime.with_llm(Arc::new(TextLlm));
                let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
                let batch = ToolBatch::for_step(
                    activation.run_id.clone(),
                    0,
                    [(
                        ToolCall {
                            call_id: "recovered-hook".into(),
                            tool_id: "echo".into(),
                            arguments: serde_json::json!({}),
                        },
                        Default::default(),
                    )],
                )
                .unwrap();
                commit
                    .commit(ThreadCommit::assemble(
                        activation.thread_id.clone(),
                        RunDisposition::running(activation.run_id.clone()),
                        true,
                        Vec::new(),
                        vec![ActiveToolBatch::write(&Some(batch))],
                        Vec::new(),
                    ))
                    .await
                    .unwrap();
                context = context.with_commit(commit.clone()).with_reader(commit);
            }
            let quiescence = context.physical_quiescence.clone();
            let task = tokio::spawn(async move { runtime.execute(activation, context).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
                .await
                .expect("phase entered");
            if matches!(mode, PhaseProbeMode::Pending) {
                task.abort();
            }
            let result = task.await;
            match mode {
                PhaseProbeMode::Return => {
                    result.expect("no panic").expect("normal run");
                }
                PhaseProbeMode::Pending => assert!(result.unwrap_err().is_cancelled()),
                PhaseProbeMode::Panic => assert!(result.unwrap_err().is_panic()),
            }
            assert_eq!(
                quiescence.can_acknowledge_return(),
                matches!(mode, PhaseProbeMode::Return),
                "{point:?}, recovered={recovered}"
            );
        }
    }
}

#[tokio::test]
async fn cancelling_a_pending_after_tool_hook_cannot_ack_the_returned_native_attempt() {
    // H3 tool invocation returned but its AfterTool participant is still pending;
    // H4 the existing Native tool-dispatch cancellation branch drops that hook.
    // H3+H4 => logical Cancelled may return, but the shared physical veto must
    // survive so Worker cannot finish_attempt. No time-based stop assertion.
    let entered = Arc::new(tokio::sync::Notify::new());
    let runtime = phase_probe_runtime(PhaseProbe {
        point: PhaseHookPoint::AfterTool,
        entered: entered.clone(),
        mode: PhaseProbeMode::Pending,
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = RuntimeRunContext::new().with_cancellation(cancellation.clone());
    let quiescence = context.physical_quiescence.clone();
    let task = tokio::spawn(async move {
        runtime
            .execute(activation(vec!["phase-probe".into()]), context)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("hook entered");
    cancellation.cancel();
    let state = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("Native returned")
        .expect("no panic")
        .expect("cancelled run");
    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    assert!(
        !quiescence.can_acknowledge_return(),
        "a dropped hook is not an observed return"
    );
}

struct ConfirmTool;

#[async_trait::async_trait]
impl ToolGateHook for ConfirmTool {
    async fn gate(&self, call: &ToolCall, _: &Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: format!("confirm-{}", call.call_id),
        })
    }
}

#[tokio::test]
async fn resumed_after_tool_hook_retains_the_same_drop_veto() {
    // H5 the tool batch is committed Awaiting, then resumed by its exact ticket.
    // H6 its AfterTool phase is entered and the resumed Future is aborted.
    // H5+H6 => the new attempt's shared latch vetoes ACK too; the first Awaiting
    // attempt did not enter that hook and therefore retains its normal return.
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
    let entered = Arc::new(tokio::sync::Notify::new());
    let runtime = phase_probe_runtime(PhaseProbe {
        point: PhaseHookPoint::AfterTool,
        entered: entered.clone(),
        mode: PhaseProbeMode::Pending,
    })
    .with_gate(Arc::new(ConfirmTool));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let original = RuntimeRunContext::new().with_commit(commit.clone());
    let original_quiescence = original.physical_quiescence.clone();
    assert_eq!(
        runtime
            .execute(activation(vec!["phase-probe".into()]), original)
            .await
            .unwrap(),
        RunState::Awaiting
    );
    assert!(original_quiescence.can_acknowledge_return());
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".into()))
        .expect("committed ticket");
    let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0);
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let quiescence = context.physical_quiescence.clone();
    let task = tokio::spawn(async move { runtime.resume(command, commit.as_ref(), context).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("resumed hook entered");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!quiescence.can_acknowledge_return());
}

impl Plugin for MarkPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "mark".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                state_keys: IdBound::Exact(vec!["phase".to_string()]),
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let mut c = Contributions::new("mark");
        c.state_keys.push("phase".to_string());
        c.phase_hooks.push(Arc::new(MarkHook));
        c
    }
}

/// A misbehaving plugin: registers a hook point it never declared in its bound.
struct OutOfBoundPlugin;

impl Plugin for OutOfBoundPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "rogue".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound::default(),
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("rogue");
        c.phase_hooks.push(Arc::new(MarkHook)); // BeforeInference, not in the empty bound
        c
    }
}

/// Requests a tool in its first Step, then ends with text — so a gate decision on
/// that one call is observable.
struct ToolThenTextLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ToolThenTextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".to_string(),
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

/// The `echo` tool — counts executions so a blocked call is provably never run.
struct CountingEcho {
    ran: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RawTool for CountingEcho {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, "echoed"))
    }
}

struct SessionToolPlugin {
    ran: Arc<AtomicUsize>,
}

impl Plugin for SessionToolPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "session-tools".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tools: IdBound::Exact(vec!["echo".to_string()]),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("session-tools");
        contributions.register_dynamic_tool(
            DynamicTool::try_new(
                ToolDescriptor::pinned(
                    "session",
                    "echo",
                    "Echo from the realized Session.",
                    serde_json::json!({"type": "object"}),
                ),
                Arc::new(CountingEcho {
                    ran: self.ran.clone(),
                }),
            )
            .expect("matching dynamic tool identity"),
        );
        contributions
    }
}

struct SessionToolLlm {
    calls: AtomicUsize,
    saw_tool: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for SessionToolLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            if request.tools.iter().any(|tool| tool.id == "echo") {
                self.saw_tool.fetch_add(1, Ordering::SeqCst);
            }
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "session-call".to_string(),
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

/// A plugin-contributed tool gate that always blocks — a pre-execution decision
/// that can only restrict, never grant (G21).
struct NarrowGate;

#[async_trait::async_trait]
impl ToolGateHook for NarrowGate {
    fn id(&self) -> &str {
        "narrow"
    }
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::Block {
            reason: "plugin policy forbids echo".to_string(),
        })
    }
}

struct NarrowGatePlugin;

impl Plugin for NarrowGatePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "narrower".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tool_gates: IdBound::Exact(vec!["narrow".to_string()]),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("narrower");
        c.tool_gates.push(Arc::new(NarrowGate));
        c
    }
}

/// An absolute host gate that always denies — the host verdict is final (G21).
/// It stands in for the authored host policy, so it names the `rule` basis.
struct DenyHostGate;

#[async_trait::async_trait]
impl ToolGateHook for DenyHostGate {
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateDecision {
        GateDecision::policy(
            GateOutcome::Block {
                reason: "host denied".to_string(),
            },
            PermissionBasis::fixed(PermissionPolicy::Rule),
        )
    }
}

/// A host gate standing in for an authored `always_allow` policy.
struct AllowHostGate;

#[async_trait::async_trait]
impl ToolGateHook for AllowHostGate {
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateDecision {
        GateDecision::policy(
            GateOutcome::Allow,
            PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
        )
    }
}

/// A plugin gate that records whether it was consulted at all — used to prove the
/// host verdict masks (short-circuits) the plugin chain.
struct RecordingGate {
    consulted: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolGateHook for RecordingGate {
    fn id(&self) -> &str {
        "recgate"
    }
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateDecision {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        GateDecision::plugin_gate(GateOutcome::Allow)
    }
}

struct RecordingGatePlugin {
    consulted: Arc<AtomicUsize>,
}

impl Plugin for RecordingGatePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "recorder".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tool_gates: IdBound::Exact(vec!["recgate".to_string()]),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("recorder");
        c.tool_gates.push(Arc::new(RecordingGate {
            consulted: self.consulted.clone(),
        }));
        c
    }
}

fn activation(plugin_ids: Vec<String>) -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation::new(
        RunId("run-1".to_string()),
        ThreadId("thread-1".to_string()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: Some(16),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("p".to_string(), "m".to_string(), "b".to_string()),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids,
                plugin_config: Default::default(),
                context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
    )
}

#[tokio::test]
async fn a_session_plugin_augments_an_immutable_publication() {
    let ran = Arc::new(AtomicUsize::new(0));
    let saw_tool = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(SessionToolLlm {
        calls: AtomicUsize::new(0),
        saw_tool: saw_tool.clone(),
    }));
    let context = RuntimeRunContext::new()
        .with_session_plugin(Arc::new(SessionToolPlugin { ran: ran.clone() }));

    let state = runtime
        .execute(activation(Vec::new()), context)
        .await
        .expect("runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        saw_tool.load(Ordering::SeqCst),
        1,
        "the model sees the Session capability without a publication rewrite"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the same plugin contribution executes the advertised tool"
    );
}

#[tokio::test]
async fn a_host_deny_masks_the_plugin_gate_chain() {
    // G21: the host permission gate is absolute. When it denies, the plugin gate
    // chain is NOT consulted — the host verdict short-circuits (masks) it, and the
    // block reason is the host's.
    let consulted = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(CountingEcho { ran: ran.clone() }))
        .with_gate(Arc::new(DenyHostGate))
        .with_plugin(Arc::new(RecordingGatePlugin {
            consulted: consulted.clone(),
        }));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation(vec!["recorder".to_string()]), context)
        .await
        .expect("runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the host-denied tool never runs"
    );
    assert_eq!(
        consulted.load(Ordering::SeqCst),
        0,
        "a host deny masks the plugin gate chain — it is never consulted"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("host denied")),
        "the block carries the host's reason"
    );
}

#[tokio::test]
async fn a_plugin_tool_gate_narrows_an_absent_host_allow() {
    // G21 gate chain: with no host permission gate (the default is Allow), a
    // plugin-contributed tool gate is still consulted (`env.tool_gates()`) and the
    // first non-Allow wins — a plugin can restrict what the host permits, never
    // widen it. The requested tool is blocked and never executes.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(CountingEcho { ran: ran.clone() }))
        .with_plugin(Arc::new(NarrowGatePlugin));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation(vec!["narrower".to_string()]), context)
        .await
        .expect("runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "a plugin-blocked tool never executes"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("blocked")),
        "the model is fed a blocked tool result"
    );
}

#[tokio::test]
async fn the_audit_names_who_decided_across_the_gate_chain() {
    // Cause/effect graph: C1 the host gate is an authored policy that allows
    // or denies; C2 a plugin gate blocks or allows; C3 a per-Run narrowing
    // (a per-Run DenyAll policy, or a tool-free activation) refuses; C4 no host
    // gate and no per-Run policy is installed. Effects: E1 exactly one
    // `PermissionDecided` row whose `decided_by` names the deciding source;
    // E2 `policy` is written iff a policy decided; E3 an allow a plugin gate
    // merely passes through is still the policy's; E4 with no gate installed
    // nothing is audited (no decider is invented).
    //
    // | Rule | host | plugin | per-Run | decision | decided_by | policy |
    // |---|---|---|---|---|---|---|
    // | W1 | allow (always_allow) | block | - | deny | plugin_gate | - |
    // | W2 | deny (rule) | allow | - | deny | policy | rule |
    // | W3 | allow (always_allow) | allow | - | allow | policy | always_allow |
    // | W4 | allow (always_allow) | allow | DenyAllTools | deny | run_narrowing | - |
    // | W5 | allow (always_allow) | allow | tool-free activation | deny | run_narrowing | - |
    // | W6 | none | block | - | (none audited) | - | - |
    type Row = (String, Option<String>, Option<String>);
    let row = |decision: &str, decided_by: &str, policy: Option<&str>| -> Row {
        (
            decision.to_string(),
            Some(decided_by.to_string()),
            policy.map(str::to_string),
        )
    };
    struct Case {
        rule: &'static str,
        host_allows: bool,
        plugin: &'static str,
        per_run_deny: bool,
        tool_free: bool,
        expected: Row,
    }
    let cases = [
        Case {
            rule: "W1",
            host_allows: true,
            plugin: "narrower",
            per_run_deny: false,
            tool_free: false,
            expected: row("deny", "plugin_gate", None),
        },
        Case {
            rule: "W2",
            host_allows: false,
            plugin: "recorder",
            per_run_deny: false,
            tool_free: false,
            expected: row("deny", "policy", Some("rule")),
        },
        Case {
            rule: "W3",
            host_allows: true,
            plugin: "recorder",
            per_run_deny: false,
            tool_free: false,
            expected: row("allow", "policy", Some("always_allow")),
        },
        Case {
            rule: "W4",
            host_allows: true,
            plugin: "recorder",
            per_run_deny: true,
            tool_free: false,
            expected: row("deny", "run_narrowing", None),
        },
        Case {
            rule: "W5",
            host_allows: true,
            plugin: "recorder",
            per_run_deny: false,
            tool_free: true,
            expected: row("deny", "run_narrowing", None),
        },
    ];
    for Case {
        rule,
        host_allows,
        plugin,
        per_run_deny,
        tool_free,
        expected,
    } in cases
    {
        let ran = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::new()
            .with_llm(Arc::new(ToolThenTextLlm {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(CountingEcho { ran: ran.clone() }))
            .with_plugin(Arc::new(NarrowGatePlugin))
            .with_plugin(Arc::new(RecordingGatePlugin {
                consulted: Arc::new(AtomicUsize::new(0)),
            }));
        runtime = if host_allows {
            runtime.with_gate(Arc::new(AllowHostGate))
        } else {
            runtime.with_gate(Arc::new(DenyHostGate))
        };
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let mut context = RuntimeRunContext::new().with_commit(commit.clone());
        if per_run_deny {
            context =
                context.with_tool_permission_policy(Arc::new(DenyAllTools::new("restricted")));
        }
        let mut activation = activation(vec![plugin.to_string()]);
        if tool_free {
            activation = activation.without_tools();
        }
        runtime.execute(activation, context).await.expect("runs");
        assert_eq!(
            audited_rows(&commit),
            vec![expected.clone()],
            "{rule}/E1/E2/E3"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            usize::from(expected.0 == "allow"),
            "{rule}: only an allow runs the tool"
        );
    }

    // W6: no host gate and no per-Run policy — the plugin still blocks, and no
    // decision is audited rather than one attributed to an invented decider.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(CountingEcho { ran: ran.clone() }))
        .with_plugin(Arc::new(NarrowGatePlugin));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    runtime
        .execute(
            activation(vec!["narrower".to_string()]),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("runs");
    assert!(audited_rows(&commit).is_empty(), "W6/E4");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "W6");
}

/// `(decision, decided_by, policy)` for every committed `PermissionDecided` row.
fn audited_rows(commit: &MemoryCommitCoordinator) -> Vec<(String, Option<String>, Option<String>)> {
    let text = |payload: &serde_json::Value, key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    commit
        .committed()
        .events
        .iter()
        .filter(|e| e.kind == awaken_agent_contract::audit::kind::Kind::PermissionDecided)
        .map(|e| {
            (
                text(&e.payload, "decision").unwrap_or_default(),
                text(&e.payload, "decided_by"),
                text(&e.payload, "policy"),
            )
        })
        .collect()
}

#[tokio::test]
async fn active_plugin_hook_stages_state_through_the_commit_path() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(vec!["mark".to_string()]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // The hook's state command was committed and is replayable.
    let store = replay_state(&commit.committed());
    assert_eq!(
        store.get(Scope::Run, &Key("phase".into())),
        Some(&serde_json::json!({
            "phase": "BeforeInference@0",
            "run_input": ["go"],
        }))
    );

    // The plugin resolved exactly once for the run.
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn inactive_plugin_contributes_nothing() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    // plugin_ids is empty: the plugin is inert and never resolved.
    runtime
        .execute(activation(Vec::new()), context)
        .await
        .expect("runs");

    // No plugin state exists; only the model's two recovery evidence records.
    assert_eq!(commit.committed().state.len(), 2);
    assert!(
        commit
            .committed()
            .state
            .iter()
            .all(|command| command.key.0 == "runtime.model_operation.v1")
    );
    assert_eq!(resolves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_selected_plugin_fails_the_actual_execution_path_closed() {
    // Inline snapshots need not pass through snapshot-file validation.  The
    // execution path itself must reject an authority request it cannot realize.
    let outcome = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .execute(
            activation(vec!["not-installed".to_string()]),
            RuntimeRunContext::new(),
        )
        .await
        .expect("configuration failure is a terminal run state");
    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound))
    );
}

#[tokio::test]
async fn duplicate_selected_plugin_identity_fails_closed_before_resolution() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let outcome = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }))
        .execute(
            activation(vec!["mark".to_string(), "mark".to_string()]),
            RuntimeRunContext::new(),
        )
        .await
        .expect("configuration failure is a terminal run state");
    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound))
    );
    assert_eq!(resolves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn out_of_bound_plugin_fails_the_run_closed() {
    // Cause/effect graph: C1=accepted activation has user input; C2=selected
    // plugin violates its capability bound before inference. E1=terminal
    // CapabilityBound; E2=no model/tool call; E3=input and terminal state commit
    // atomically. Decision rule P1 C1+C2 -> E1+E2+E3.
    // FMECA: committing only the error loses the accepted user message and makes
    // Host completion proof replace the domain error with missing-input
    // (critical). Mitigation: every pre-inference terminal commit carries the
    // activation input through the same `finish` boundary.
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(OutOfBoundPlugin));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(vec!["rogue".to_string()]), context)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
        "a contribution outside the declared bound fails closed (G30)"
    );
    assert_eq!(
        commit
            .committed()
            .messages
            .iter()
            .map(|message| message.id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["m1"],
        "a pre-inference capability failure durably identifies its accepted input"
    );
}
