#![allow(missing_docs)]
//! Comprehensive tool executor and agent loop tests ported from uncarve.
//! Covers: sequential/parallel execution, suspension, state machine transitions,
//! cross-hook state visibility, stop conditions, and phase interaction.

use async_trait::async_trait;
use awaken::agent::state::{
    ContextThrottleState, RunLifecycle, SetInferenceOverride, ToolCallStates,
};
use awaken::contract::content::ContentBlock;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::{NullEventSink, VecEventSink};
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::lifecycle::{RunStatus, TerminationReason};
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::suspension::{ToolCallOutcome, ToolCallStatus};
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::execution::ParallelToolExecutor;
use awaken::loop_runner::{AgentLoopParams, build_agent_env, run_agent_loop};
use awaken::registry::AgentSpec;
use awaken::*;
use awaken::{AgentResolver, ExecutionEnv, ResolvedAgent, RuntimeError};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(StreamResult {
                content: vec![ContentBlock::text("Nothing more.")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(responses.remove(0))
        }
    }
    fn name(&self) -> &str {
        "scripted"
    }
}

// ---------------------------------------------------------------------------
// Mock Tools
// ---------------------------------------------------------------------------

struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "echo", "Echoes")
    }
    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("echo")
            .to_string();
        Ok(ToolResult::success_with_message("echo", args, msg).into())
    }
}

struct FailingTool;
#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("failing", "failing", "Fails")
    }
    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::ExecutionFailed("intentional failure".into()))
    }
}

struct SuspendingTool;
#[async_trait]
impl Tool for SuspendingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("suspending", "suspending", "Suspends")
    }
    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::suspended("suspending", "needs approval").into())
    }
}

struct CountingTool {
    call_count: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for CountingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("counting", "counting", "Counts")
    }
    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ToolResult::success("counting", json!({"count": n})).into())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use awaken::agent::state::ContextMessageStore;

struct LoopStatePlugin;
impl Plugin for LoopStatePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "loop-state" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions::default())?;
        r.register_key::<ContextThrottleState>(StateKeyOptions::default())?;
        r.register_key::<ContextMessageStore>(StateKeyOptions::default())?;
        Ok(())
    }
}

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let rt = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    rt
}

/// Test resolver that wraps a fixed ResolvedAgent + optional user plugins.
struct FixedResolver {
    agent: ResolvedAgent,
    user_plugins: Vec<Arc<dyn Plugin>>,
}

impl FixedResolver {
    fn new(agent: ResolvedAgent) -> Self {
        Self {
            agent,
            user_plugins: vec![],
        }
    }

    fn with_plugins(agent: ResolvedAgent, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        Self {
            agent,
            user_plugins: plugins,
        }
    }
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&self.user_plugins, &agent)?;
        Ok(agent)
    }
}

fn id() -> RunIdentity {
    RunIdentity::new(
        "t1".into(),
        None,
        "r1".into(),
        None,
        "agent".into(),
        RunOrigin::User,
    )
}

fn tool_step(calls: Vec<ToolCall>) -> StreamResult {
    StreamResult {
        content: vec![],
        tool_calls: calls,
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }
}

fn text_step(text: &str) -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text(text)],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }
}

// ===========================================================================
// SEQUENTIAL EXECUTOR TESTS
// ===========================================================================

#[tokio::test]
async fn sequential_partial_failure_both_produce_results() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "failing", json!({})),
        ]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = e {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.len(), 2);
    let by_id: std::collections::HashMap<_, _> = tool_dones.into_iter().collect();
    assert_eq!(by_id.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(by_id.get("c2"), Some(&ToolCallOutcome::Failed));
}

#[tokio::test]
async fn sequential_stops_after_first_suspension_in_loop() {
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "suspending", json!({})),
        ToolCall::new("c2", "echo", json!({"message": "should not run"})),
    ])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::Suspended);
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // The second tool should not execute, but its call is backfilled as interrupted.
    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = e {
                Some((id.as_str(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        tool_dones,
        vec![
            ("c1", ToolCallOutcome::Suspended),
            ("c2", ToolCallOutcome::Failed),
        ]
    );
}

// ===========================================================================
// PARALLEL EXECUTOR TESTS
// ===========================================================================

#[tokio::test]
async fn parallel_both_tools_execute() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "echo", json!({"message": "first"})),
            ToolCall::new("c2", "echo", json!({"message": "second"})),
        ]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .collect();
    assert_eq!(tool_dones.len(), 2);
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn parallel_partial_failure() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "failing", json!({})),
        ]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let events = sink.take();
    let outcomes: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone { outcome, .. } = e {
                Some(*outcome)
            } else {
                None
            }
        })
        .collect();
    assert!(outcomes.contains(&ToolCallOutcome::Succeeded));
    assert!(outcomes.contains(&ToolCallOutcome::Failed));
}

#[tokio::test]
async fn parallel_does_not_stop_on_suspension() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "suspending", json!({})),
        ToolCall::new("c2", "counting", json!({})),
    ])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(CountingTool {
            call_count: call_count.clone(),
        }))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Both tools executed (parallel doesn't stop on suspension)
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "counting tool should have executed"
    );
    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .collect();
    assert_eq!(tool_dones.len(), 2);
    // Run should be suspended since at least one tool suspended
    assert_eq!(result.termination, TerminationReason::Suspended);
}

// ===========================================================================
// SUSPENSION LIFECYCLE TESTS
// ===========================================================================

#[tokio::test]
async fn suspension_sets_run_to_waiting() {
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![ToolCall::new(
        "c1",
        "suspending",
        json!({}),
    )])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::Suspended);
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);
}

#[tokio::test]
async fn suspension_tool_call_state_is_suspended() {
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![ToolCall::new(
        "c1",
        "suspending",
        json!({}),
    )])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let tool_states = rt.store().read::<ToolCallStates>().unwrap_or_default();
    let c1 = tool_states.calls.get("c1").expect("c1 should exist");
    assert_eq!(c1.status, ToolCallStatus::Suspended);
}

// ===========================================================================
// CROSS-HOOK STATE VISIBILITY TESTS
// ===========================================================================

#[tokio::test]
async fn hook_state_mutation_is_not_visible_to_sibling_hook() {
    // Hooks gather against the same snapshot, so sibling writes are not visible.
    struct WriterHook;
    #[async_trait]
    impl PhaseHook for WriterHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.update::<TestCounter>(1);
            Ok(cmd)
        }
    }

    struct ReaderHook {
        observed: Arc<Mutex<Option<usize>>>,
    }
    #[async_trait]
    impl PhaseHook for ReaderHook {
        async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let val = ctx.state::<TestCounter>().copied().unwrap_or(0);
            *self.observed.lock().unwrap() = Some(val);
            Ok(StateCommand::new())
        }
    }

    struct TestCounter;
    impl StateKey for TestCounter {
        const KEY: &'static str = "test.counter";
        type Value = usize;
        type Update = usize;
        fn apply(value: &mut usize, update: usize) {
            *value += update;
        }
    }

    struct HookPlugin {
        observed: Arc<Mutex<Option<usize>>>,
    }
    impl Plugin for HookPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "hook-test" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<TestCounter>(StateKeyOptions::default())?;
            // Writer first, then reader
            r.register_phase_hook("hook-test", Phase::BeforeInference, WriterHook)?;
            r.register_phase_hook(
                "hook-test",
                Phase::BeforeInference,
                ReaderHook {
                    observed: self.observed.clone(),
                },
            )?;
            Ok(())
        }
    }

    let observed = Arc::new(Mutex::new(None));
    let llm = Arc::new(ScriptedLlm::new(vec![text_step("done")]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let rt = make_runtime();
    rt.store()
        .install_plugin(HookPlugin {
            observed: observed.clone(),
        })
        .unwrap();

    let hook_plugin = Arc::new(HookPlugin {
        observed: observed.clone(),
    });
    let user_plugins: Vec<Arc<dyn Plugin>> = vec![hook_plugin];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(*observed.lock().unwrap(), Some(0));
    assert_eq!(rt.store().read::<TestCounter>(), Some(1));
}

// ===========================================================================
// STOP CONDITION TESTS
// ===========================================================================

#[tokio::test]
async fn max_rounds_precise_count() {
    let llm = Arc::new(ScriptedLlm::new(
        (0..10)
            .map(|i| {
                tool_step(vec![ToolCall::new(
                    format!("c{i}"),
                    "echo",
                    json!({"message": "loop"}),
                )])
            })
            .collect(),
    ));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_max_rounds(3)
        .with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert!(
        matches!(result.termination, TerminationReason::Stopped(ref s) if s.code == "max_rounds")
    );
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(
        lifecycle.step_count, 3,
        "should complete exactly 3 steps before stopping"
    );
}

#[tokio::test]
async fn terminate_via_state_in_after_inference_hook() {
    use awaken::agent::state::{RunLifecycle, RunLifecycleUpdate};

    struct TerminateHook;
    #[async_trait]
    impl PhaseHook for TerminateHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.update::<RunLifecycle>(RunLifecycleUpdate::Done {
                done_reason: "stopped:custom_stop".into(),
                updated_at: 0,
            });
            Ok(cmd)
        }
    }

    struct TermHookPlugin;
    impl Plugin for TermHookPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "term-hook" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_phase_hook("term-hook", Phase::AfterInference, TerminateHook)?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new("c1", "echo", json!({}))]),
        text_step("should not reach"),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(TermHookPlugin)];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert!(
        matches!(result.termination, TerminationReason::Stopped(ref s) if s.code == "custom_stop")
    );
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

// ===========================================================================
// PHASE SEQUENCE TESTS
// ===========================================================================

#[tokio::test]
async fn phase_sequence_with_tool_call() {
    let phases = Arc::new(Mutex::new(Vec::<Phase>::new()));
    struct PhaseLogger(Arc<Mutex<Vec<Phase>>>);
    #[async_trait]
    impl PhaseHook for PhaseLogger {
        async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            self.0.lock().unwrap().push(ctx.phase);
            Ok(StateCommand::new())
        }
    }

    struct LogPlugin(Arc<Mutex<Vec<Phase>>>);
    impl Plugin for LogPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "logger" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            for phase in Phase::ALL {
                r.register_phase_hook("logger", phase, PhaseLogger(self.0.clone()))?;
            }
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(LogPlugin(phases.clone()))];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let p = phases.lock().unwrap();
    assert_eq!(p[0], Phase::RunStart);
    // Step 1: tool call
    assert_eq!(p[1], Phase::StepStart);
    assert_eq!(p[2], Phase::BeforeInference);
    assert_eq!(p[3], Phase::AfterInference);
    assert_eq!(p[4], Phase::BeforeToolExecute);
    assert_eq!(p[5], Phase::AfterToolExecute);
    assert_eq!(p[6], Phase::StepEnd);
    // Step 2: final response
    assert_eq!(p[7], Phase::StepStart);
    assert_eq!(p[8], Phase::BeforeInference);
    assert_eq!(p[9], Phase::AfterInference);
    assert_eq!(p[10], Phase::StepEnd);
    assert_eq!(p[11], Phase::RunEnd);
}

#[tokio::test]
async fn phase_sequence_on_suspension() {
    let phases = Arc::new(Mutex::new(Vec::<Phase>::new()));
    struct PhaseLogger(Arc<Mutex<Vec<Phase>>>);
    #[async_trait]
    impl PhaseHook for PhaseLogger {
        async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            self.0.lock().unwrap().push(ctx.phase);
            Ok(StateCommand::new())
        }
    }
    struct LogPlugin(Arc<Mutex<Vec<Phase>>>);
    impl Plugin for LogPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "logger" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            for phase in Phase::ALL {
                r.register_phase_hook("logger", phase, PhaseLogger(self.0.clone()))?;
            }
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![ToolCall::new(
        "c1",
        "suspending",
        json!({}),
    )])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(LogPlugin(phases.clone()))];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let p = phases.lock().unwrap();
    assert_eq!(p[0], Phase::RunStart);
    assert_eq!(p[1], Phase::StepStart);
    assert_eq!(p[2], Phase::BeforeInference);
    assert_eq!(p[3], Phase::AfterInference);
    assert_eq!(p[4], Phase::BeforeToolExecute);
    assert_eq!(p[5], Phase::AfterToolExecute);
    assert_eq!(p[6], Phase::StepEnd);
    assert_eq!(p[7], Phase::RunEnd);
}

// ===========================================================================
// PROFILE SECTIONS ACCESSIBLE IN HOOKS DURING LOOP
// ===========================================================================

#[tokio::test]
async fn profile_sections_available_in_loop_hooks() {
    let observed = Arc::new(Mutex::new(String::new()));
    struct ConfigReader(Arc<Mutex<String>>);
    #[async_trait]
    impl PhaseHook for ConfigReader {
        async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let name = ctx
                .agent_spec
                .sections
                .get("test.model")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            *self.0.lock().unwrap() = name;
            Ok(StateCommand::new())
        }
    }
    struct CfgPlugin(Arc<Mutex<String>>);
    impl Plugin for CfgPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "cfg-reader" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_phase_hook(
                "cfg-reader",
                Phase::BeforeInference,
                ConfigReader(self.0.clone()),
            )?;
            Ok(())
        }
    }

    let rt = make_runtime();

    let cfg_plugin = Arc::new(CfgPlugin(observed.clone()));
    let env =
        ExecutionEnv::from_plugins(&[cfg_plugin as Arc<dyn Plugin>], &Default::default()).unwrap();

    // Build a spec with the test section
    let spec = std::sync::Arc::new(
        AgentSpec::new("test")
            .with_section("test.model", serde_json::json!({"name": "test-model"})),
    );

    // Run phases manually with spec context
    let store = rt.store();
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot()).with_agent_spec(spec);
    rt.run_phase_with_context(&env, ctx).await.unwrap();

    assert_eq!(*observed.lock().unwrap(), "test-model");
}

// ===========================================================================
// EDGE CASES
// ===========================================================================

#[tokio::test]
async fn empty_tool_calls_treated_as_natural_end() {
    // LLM returns tool_calls = [] with ToolUse stop_reason (edge case)
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Just text.")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::ToolUse), // misleading stop_reason
        has_incomplete_tool_calls: false,
    }]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.response, "Just text.");
}

#[tokio::test]
async fn multiple_steps_accumulate_messages() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new(
            "c1",
            "echo",
            json!({"message": "step1"}),
        )]),
        tool_step(vec![ToolCall::new(
            "c2",
            "echo",
            json!({"message": "step2"}),
        )]),
        text_step("Final answer."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.steps, 3);
    assert_eq!(result.response, "Final answer.");
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.step_count, 3);
}

#[tokio::test]
async fn run_lifecycle_run_id_matches_identity() {
    let llm = Arc::new(ScriptedLlm::new(vec![text_step("ok")]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let custom_id = RunIdentity::new(
        "t-x".into(),
        None,
        "r-x".into(),
        None,
        "a-x".into(),
        RunOrigin::Internal,
    );
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: custom_id,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.run_id, "r-x");
}

// ===========================================================================
// PARALLEL BATCH APPROVAL INTEGRATION TESTS
// ===========================================================================

#[tokio::test]
async fn batch_approval_both_tools_execute_in_loop() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "echo", json!({"message": "a"})),
            ToolCall::new("c2", "echo", json!({"message": "b"})),
        ]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::batch_approval()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .collect();
    assert_eq!(tool_dones.len(), 2);
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn batch_approval_suspension_still_executes_all() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "suspending", json!({})),
        ToolCall::new("c2", "counting", json!({})),
    ])]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(CountingTool {
            call_count: call_count.clone(),
        }))
        .with_tool_executor(Arc::new(ParallelToolExecutor::batch_approval()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "counting tool should have executed even with batch approval"
    );
    assert_eq!(result.termination, TerminationReason::Suspended);
}

// ===========================================================================
// PARALLEL STREAMING INTEGRATION TESTS
// ===========================================================================

#[tokio::test]
async fn streaming_partial_failure_in_loop() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "failing", json!({})),
        ]),
        text_step("Done."),
    ]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let events = sink.take();
    let outcomes: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone { outcome, .. } = e {
                Some(*outcome)
            } else {
                None
            }
        })
        .collect();
    assert!(outcomes.contains(&ToolCallOutcome::Succeeded));
    assert!(outcomes.contains(&ToolCallOutcome::Failed));
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
}

// ===========================================================================
// INFERENCE OVERRIDE VIA EFFECT
// ===========================================================================

#[tokio::test]
async fn before_inference_hook_override_reaches_request() {
    use awaken::contract::inference::InferenceOverride;

    // LLM that captures the InferenceRequest
    struct CapturingLlm {
        captured: Mutex<Vec<Option<InferenceOverride>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.captured.lock().unwrap().push(req.overrides);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    // Plugin that emits InferenceOverride in BeforeInference
    struct OverridePlugin;
    impl Plugin for OverridePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "override-plugin",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook;
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    cmd.schedule_action::<SetInferenceOverride>(InferenceOverride {
                        temperature: Some(0.0),
                        max_tokens: Some(256),
                        ..Default::default()
                    })?;
                    Ok(cmd)
                }
            }
            r.register_phase_hook("override-plugin", Phase::BeforeInference, Hook)?;
            Ok(())
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone());
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OverridePlugin)];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let overrides = captured[0].as_ref().expect("overrides should be set");
    assert_eq!(overrides.temperature, Some(0.0));
    assert_eq!(overrides.max_tokens, Some(256));
    assert!(overrides.model.is_none());
}

#[tokio::test]
async fn multiple_hooks_merge_inference_overrides_last_wins() {
    use awaken::contract::inference::InferenceOverride;

    struct CapturingLlm {
        captured: Mutex<Vec<Option<InferenceOverride>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.captured.lock().unwrap().push(req.overrides);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    // Plugin A sets temperature=0.5, model="model-a"
    struct OverridePluginA;
    impl Plugin for OverridePluginA {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "override-a" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook;
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    cmd.schedule_action::<SetInferenceOverride>(InferenceOverride {
                        temperature: Some(0.5),
                        model: Some("model-a".into()),
                        ..Default::default()
                    })?;
                    Ok(cmd)
                }
            }
            r.register_phase_hook("override-a", Phase::BeforeInference, Hook)?;
            Ok(())
        }
    }

    // Plugin B sets temperature=0.9, max_tokens=512 (overwrites A's temperature, A's model preserved)
    struct OverridePluginB;
    impl Plugin for OverridePluginB {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "override-b" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook;
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    cmd.schedule_action::<SetInferenceOverride>(InferenceOverride {
                        temperature: Some(0.9),
                        max_tokens: Some(512),
                        ..Default::default()
                    })?;
                    Ok(cmd)
                }
            }
            r.register_phase_hook("override-b", Phase::BeforeInference, Hook)?;
            Ok(())
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone());
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(OverridePluginA), Arc::new(OverridePluginB)];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(NullEventSink),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let overrides = captured[0].as_ref().expect("overrides should be set");
    // B's temperature wins (last-wins)
    assert_eq!(overrides.temperature, Some(0.9));
    // B's max_tokens set
    assert_eq!(overrides.max_tokens, Some(512));
    // A's model preserved (B didn't set it)
    assert_eq!(overrides.model.as_deref(), Some("model-a"));
}

#[tokio::test]
async fn no_override_hook_leaves_overrides_none() {
    use awaken::contract::inference::InferenceOverride;

    struct CapturingLlm {
        captured: Mutex<Vec<Option<InferenceOverride>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.captured.lock().unwrap().push(req.overrides);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone());
    let rt = make_runtime();

    // No override plugins
    let resolver = FixedResolver::new(agent);

    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(NullEventSink),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].is_none(),
        "overrides should be None when no hook emits override"
    );
}

#[tokio::test]
async fn override_consumed_each_step_not_leaked() {
    use awaken::contract::inference::InferenceOverride;

    struct CapturingLlm {
        captured: Mutex<Vec<Option<InferenceOverride>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let call_idx = self.captured.lock().unwrap().len();
            self.captured.lock().unwrap().push(req.overrides);
            // First call: return tool call to force a second step
            if call_idx == 0 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("done")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    // Plugin that ONLY emits override on the first BeforeInference call
    struct OnceOverridePlugin {
        emitted: Arc<AtomicUsize>,
    }
    impl Plugin for OnceOverridePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "once-override",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook(Arc<AtomicUsize>);
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    // Only emit on first call
                    if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                        cmd.schedule_action::<SetInferenceOverride>(InferenceOverride {
                            temperature: Some(0.0),
                            ..Default::default()
                        })?;
                    }
                    Ok(cmd)
                }
            }
            r.register_phase_hook(
                "once-override",
                Phase::BeforeInference,
                Hook(self.emitted.clone()),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone()).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let emitted = Arc::new(AtomicUsize::new(0));
    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OnceOverridePlugin {
        emitted: emitted.clone(),
    })];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(NullEventSink),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "should have two inference calls");
    // First call: override set
    assert!(captured[0].is_some(), "first call should have override");
    assert_eq!(captured[0].as_ref().unwrap().temperature, Some(0.0));
    // Second call: override consumed, not leaked
    assert!(
        captured[1].is_none(),
        "second call should NOT have override (consumed)"
    );
}

// ===========================================================================
// CONTEXT MESSAGE INJECTION VIA ACTION
// ===========================================================================

#[tokio::test]
async fn context_message_injected_into_request() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    // LLM that captures the messages it receives
    struct CapturingLlm {
        captured: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.captured.lock().unwrap().push(req.messages);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    // Plugin that injects a system context message and a suffix system message
    struct ContextPlugin;
    impl Plugin for ContextPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "context-plugin",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook;
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                        "ctx.assistant",
                        "You are a helpful assistant",
                    ))?;
                    cmd.schedule_action::<AddContextMessage>(ContextMessage::suffix_system(
                        "ctx.concise",
                        "Remember: be concise",
                    ))?;
                    Ok(cmd)
                }
            }
            r.register_phase_hook("context-plugin", Phase::BeforeInference, Hook)?;
            Ok(())
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone());
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(ContextPlugin)];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(NullEventSink),
        checkpoint_store: None,
        messages: vec![Message::user("hello")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let msgs = &captured[0];

    // Expected order:
    // [0] system prompt ("sys" from ResolvedAgent)
    // [1] system context message ("You are a helpful assistant") — System target, after base
    // [2] user message ("hello")
    // [3] suffix system ("Remember: be concise") — SuffixSystem target, at end
    assert!(
        msgs.len() >= 4,
        "should have at least 4 messages, got {}",
        msgs.len()
    );

    // Base system prompt
    assert_eq!(msgs[0].role, awaken::contract::message::Role::System);
    assert_eq!(msgs[0].text(), "sys");

    // Injected system context
    assert_eq!(msgs[1].role, awaken::contract::message::Role::System);
    assert_eq!(msgs[1].text(), "You are a helpful assistant");

    // User message
    assert_eq!(msgs[2].role, awaken::contract::message::Role::User);
    assert_eq!(msgs[2].text(), "hello");

    // Suffix system
    let last = msgs.last().unwrap();
    assert_eq!(last.role, awaken::contract::message::Role::System);
    assert_eq!(last.text(), "Remember: be concise");
}

#[tokio::test]
async fn context_messages_not_leaked_to_next_step() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    struct CapturingLlm {
        captured: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let call_idx = self.captured.lock().unwrap().len();
            self.captured.lock().unwrap().push(req.messages);
            if call_idx == 0 {
                // First call: return tool call to force second step
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("done")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    // Plugin that only injects on first step
    struct OnceContextPlugin {
        emitted: Arc<AtomicUsize>,
    }
    impl Plugin for OnceContextPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "once-context",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct Hook(Arc<AtomicUsize>);
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                        cmd.schedule_action::<AddContextMessage>(ContextMessage::suffix_system(
                            "ctx.once",
                            "first step only",
                        ))?;
                    }
                    Ok(cmd)
                }
            }
            r.register_phase_hook(
                "once-context",
                Phase::BeforeInference,
                Hook(self.emitted.clone()),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(CapturingLlm {
        captured: Mutex::new(Vec::new()),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone()).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let emitted = Arc::new(AtomicUsize::new(0));
    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OnceContextPlugin {
        emitted: emitted.clone(),
    })];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(NullEventSink),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let captured = llm.captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "should have two inference calls");

    // First call: should have the suffix context message
    let first_msgs = &captured[0];
    assert!(
        first_msgs.iter().any(|m| m.text() == "first step only"),
        "first call should have injected context message"
    );

    // Second call: should NOT have the context message (action consumed, not re-scheduled)
    let second_msgs = &captured[1];
    assert!(
        !second_msgs.iter().any(|m| m.text() == "first step only"),
        "second call should NOT have context message (not leaked)"
    );
}
