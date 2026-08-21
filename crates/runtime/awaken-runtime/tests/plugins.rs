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
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
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
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
        GateOutcome::Block {
            reason: "plugin policy forbids echo".to_string(),
        }
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
struct DenyHostGate;

#[async_trait::async_trait]
impl ToolGateHook for DenyHostGate {
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
        GateOutcome::Block {
            reason: "host denied".to_string(),
        }
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
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        GateOutcome::Allow
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
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "p".to_string(),
                        model_ref: "m".to_string(),
                        backend_ref: "b".to_string(),
                    },
                ),
                tool_descriptors: Vec::new(),
                plugin_ids,
                plugin_config: Default::default(),
                context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
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
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
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

    let commit = Arc::new(MemoryCommitCoordinator::new());
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

    let commit = Arc::new(MemoryCommitCoordinator::new());
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
async fn active_plugin_hook_stages_state_through_the_commit_path() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }));

    let commit = Arc::new(MemoryCommitCoordinator::new());
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

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    // plugin_ids is empty: the plugin is inert and never resolved.
    runtime
        .execute(activation(Vec::new()), context)
        .await
        .expect("runs");

    assert!(commit.committed().state.is_empty());
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

    let commit = Arc::new(MemoryCommitCoordinator::new());
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
        "P1"
    );
}
