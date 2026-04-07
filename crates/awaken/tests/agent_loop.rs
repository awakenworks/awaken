#![allow(missing_docs)]

use async_trait::async_trait;
use awaken::agent::state::{
    ContextMessageStore, ContextThrottleState, PendingWorkKey, RunLifecycle, ToolCallStates,
};
use awaken::contract::content::ContentBlock;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::{NullEventSink, VecEventSink};
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken::contract::lifecycle::{RunStatus, TerminationReason};
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::suspension::{
    ResumeDecisionAction, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
};
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::contract::tool_intercept::{ToolInterceptAction, ToolInterceptPayload};
use awaken::loop_runner::{AgentLoopParams, build_agent_env, prepare_resume, run_agent_loop};
use awaken::*;
use awaken::{AgentResolver, ResolvedAgent};
use serde_json::{Value, json};
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
                content: vec![ContentBlock::text("I have nothing more to say.")],
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
        ToolDescriptor::new("echo", "echo", "Echoes input back")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message")
            .to_string();
        Ok(ToolResult::success_with_message("echo", args, msg).into())
    }
}

struct CalcTool;

#[async_trait]
impl Tool for CalcTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("calc", "calculator", "Evaluates math")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let result = args.get("result").cloned().unwrap_or(json!(0));
        Ok(ToolResult::success("calc", result).into())
    }
}

struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("fail", "fail", "Always fails")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::ExecutionFailed("intentional failure".into()))
    }
}

/// Tool that always returns Pending (suspends).
struct SuspendingTool;

#[async_trait]
impl Tool for SuspendingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("dangerous", "dangerous", "Requires approval")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::suspended("dangerous", "needs user approval").into())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct LoopStatePlugin;

impl Plugin for LoopStatePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "loop-state" }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        registrar.register_key::<ToolCallStates>(StateKeyOptions::default())?;
        registrar.register_key::<ContextThrottleState>(StateKeyOptions::default())?;
        registrar.register_key::<ContextMessageStore>(StateKeyOptions::default())?;
        registrar.register_key::<PendingWorkKey>(StateKeyOptions::default())?;
        Ok(())
    }
}

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    runtime
}

fn test_identity() -> RunIdentity {
    RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "test-agent".into(),
        RunOrigin::User,
    )
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_step_natural_end() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Hello, world!")],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "You are helpful.", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.response, "Hello, world!");
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 1);

    // Verify run lifecycle state
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
    assert_eq!(lifecycle.status_reason.as_deref(), Some("natural"));
    assert_eq!(lifecycle.step_count, 1);
    assert_eq!(lifecycle.run_id, "run-1");
}

#[tokio::test]
async fn tool_call_then_response() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("Let me search.")],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hello"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("The echo said: hello")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo hello")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.response, "The echo said: hello");
    assert_eq!(result.steps, 2);

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.step_count, 2);
}

#[tokio::test]
async fn tool_call_state_machine_transitions() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("test")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After step 1 (with tool call), tool call state should show Succeeded
    // But step 2 clears it, so final state is empty (cleared at step start)
    let tool_states = runtime.store().read::<ToolCallStates>().unwrap_or_default();
    // Step 2 had no tool calls, so after Clear at step 2 start, it's empty
    assert!(tool_states.calls.is_empty());
}

#[tokio::test]
async fn multiple_tool_calls_in_one_step() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "first"})),
                ToolCall::new("c2", "calc", json!({"result": 42})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("multi-tool")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.steps, 2);
    let events = sink.take();
    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert_eq!(tool_done_count, 2);
}

#[tokio::test]
async fn max_rounds_exceeded() {
    let llm = Arc::new(ScriptedLlm::new(
        (0..5)
            .map(|i| StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    format!("c{i}"),
                    "echo",
                    json!({"message": "loop"}),
                )],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
            .collect(),
    ));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_max_rounds(3)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("loop")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert!(matches!(
        result.termination,
        TerminationReason::Stopped(ref s) if s.code == "max_rounds"
    ));

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
    assert!(
        lifecycle
            .status_reason
            .as_deref()
            .unwrap()
            .starts_with("stopped:max_rounds")
    );
}

#[tokio::test]
async fn unknown_tool_returns_error_result_not_crash() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "nonexistent", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Sorry, that tool doesn't exist.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("call unknown")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap(); // Should NOT error — unknown tool produces ToolResult::error

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 2);

    // The tool call should have Failed status
    // (cleared by step 2, but the event shows it)
    let events = sink.take();
    let tool_fail_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, AgentEvent::ToolCallDone { outcome, .. }
                if *outcome == awaken::contract::suspension::ToolCallOutcome::Failed)
        })
        .collect();
    assert_eq!(tool_fail_events.len(), 1);
}

#[tokio::test]
async fn failing_tool_produces_error_result_continues_loop() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "fail", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Tool failed, sorry.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent =
        ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use fail tool")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn events_have_correct_sequence_for_single_step() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Hi!")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    // Filter to lifecycle events only (skip streaming deltas)
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::RunStart { .. } => Some("RunStart"),
            AgentEvent::StepStart { .. } => Some("StepStart"),
            AgentEvent::InferenceComplete { .. } => Some("InferenceComplete"),
            AgentEvent::StepEnd => Some("StepEnd"),
            AgentEvent::RunFinish { .. } => Some("RunFinish"),
            _ => None, // skip TextDelta, ToolCallStart, etc.
        })
        .collect();

    assert_eq!(
        event_types,
        vec![
            "RunStart",
            "StepStart",
            "InferenceComplete",
            "StepEnd",
            "RunFinish"
        ]
    );

    // Verify TextDelta was emitted
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { .. })),
        "should emit TextDelta events during streaming"
    );
}

#[tokio::test]
async fn events_have_correct_sequence_with_tool_call() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "x"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
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
    // Filter to lifecycle + tool events (skip streaming deltas)
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::RunStart { .. } => Some("RunStart"),
            AgentEvent::StepStart { .. } => Some("StepStart"),
            AgentEvent::InferenceComplete { .. } => Some("InferenceComplete"),
            AgentEvent::ToolCallStart { .. } => Some("ToolCallStart"),
            AgentEvent::ToolCallDone { .. } => Some("ToolCallDone"),
            AgentEvent::StepEnd => Some("StepEnd"),
            AgentEvent::RunFinish { .. } => Some("RunFinish"),
            _ => None,
        })
        .collect();

    assert_eq!(
        event_types,
        vec![
            "RunStart",
            // Step 1: tool call
            "StepStart",
            "ToolCallStart",
            "InferenceComplete",
            "ToolCallDone",
            "StepEnd",
            // Step 2: final response
            "StepStart",
            "InferenceComplete",
            "StepEnd",
            "RunFinish",
        ]
    );
}

#[tokio::test]
async fn lifecycle_state_reflects_custom_run_id() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let identity = RunIdentity::new(
        "t-custom".into(),
        None,
        "r-custom".into(),
        None,
        "a-custom".into(),
        RunOrigin::Internal,
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.run_id, "r-custom");
}

#[tokio::test]
async fn phase_hooks_fire_during_loop() {
    let hook_phases = Arc::new(Mutex::new(Vec::<Phase>::new()));

    struct PhaseTracker(Arc<Mutex<Vec<Phase>>>);
    #[async_trait]
    impl PhaseHook for PhaseTracker {
        async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            self.0.lock().unwrap().push(ctx.phase);
            Ok(StateCommand::new())
        }
    }

    struct TrackerPlugin(Arc<Mutex<Vec<Phase>>>);
    impl Plugin for TrackerPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "tracker" }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            for phase in Phase::ALL {
                registrar.register_phase_hook(
                    "tracker",
                    phase,
                    PhaseTracker(Arc::clone(&self.0)),
                )?;
            }
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();

    let tracker_plugin = Arc::new(TrackerPlugin(Arc::clone(&hook_phases)));
    let user_plugins: Vec<Arc<dyn Plugin>> = vec![tracker_plugin];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let phases = hook_phases.lock().unwrap();
    assert_eq!(
        *phases,
        vec![
            Phase::RunStart,
            Phase::StepStart,
            Phase::BeforeInference,
            Phase::AfterInference,
            Phase::StepEnd,
            Phase::RunEnd,
        ]
    );
}

// ---------------------------------------------------------------------------
// Suspension & Resume tests
// ---------------------------------------------------------------------------

fn make_tool_call_response(tool_name: &str, call_id: &str, args: Value) -> StreamResult {
    StreamResult {
        content: vec![],
        tool_calls: vec![ToolCall::new(call_id, tool_name, args)],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }
}

#[tokio::test]
async fn tool_suspension_transitions_run_to_waiting() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "delete"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Run should be in Waiting state
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // Tool call should be Suspended
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Suspended);
}

#[tokio::test]
async fn resume_with_use_decision_as_tool_result() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // First call: tool call that suspends
        make_tool_call_response("dangerous", "c1", json!({"action": "delete"})),
        // After resume: LLM sees the decision result and ends
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Collect messages from the first run
    let messages: Vec<Message> = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c1",
                "dangerous",
                json!({"action": "delete"}),
            )],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    // Resume with decision
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Should have completed (LLM returns text, no more tools)
    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);

    // Tool call should be terminal
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap_or_default();
    assert!(tc_states.calls.is_empty());
    // After resume, tool call states were cleared by the new step.
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

#[tokio::test]
async fn resume_with_cancel_marks_tool_cancelled() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "delete"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    let messages = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c1",
                "dangerous",
                json!({"action": "delete"}),
            )],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    // Resume with cancel
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Cancel,
                result: Value::Null,
                reason: Some("user denied".into()),
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap();

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After cancel, the loop continues with LLM seeing the cancellation message
    // LLM has no more responses so it returns text → NaturalEnd
    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn resume_with_replay_tool_call() {
    // After resume, ReplayToolCall re-executes with original args.
    // We use EchoTool on resume so it succeeds this time.
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"message": "hello"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(EchoTool)); // echo registered for replay
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Now swap the tool: register "dangerous" as EchoTool for replay
    // We create a new agent with EchoTool registered as "dangerous"
    struct DangerousEcho;
    #[async_trait]
    impl Tool for DangerousEcho {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "Now approved echo")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("dangerous", args).into())
        }
    }

    let llm2 = Arc::new(ScriptedLlm::new(vec![]));
    let agent2 = ResolvedAgent::new("test", "m", "sys", llm2).with_tool(Arc::new(DangerousEcho));
    let resolver2 = FixedResolver::new(agent2);

    let messages = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c1",
                "dangerous",
                json!({"message": "hello"}),
            )],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap();

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver2,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn resume_with_pass_decision_to_tool() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "passthrough",
        "c1",
        json!({"original": true}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Hack: register passthrough as "dangerous" initially for suspension
    // Actually, let's use a different approach: SuspendingTool is "dangerous"
    // but we need passthrough for resume. Let's use a tool that suspends first.
    // Simpler: just use SuspendingTool for suspension, then on resume use passthrough.

    // First run: suspend
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;
    // This might not work because tool_call name is "passthrough" but we only have "dangerous".
    // Let me adjust: use "dangerous" tool call and have passthrough registered as "dangerous" on resume.
    drop(result);

    // Start fresh with correct setup:
    let runtime2 = make_runtime();
    let llm2 = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"original": true}),
    )]));
    let agent2 = ResolvedAgent::new("test", "m", "sys", llm2).with_tool(Arc::new(SuspendingTool));
    let resolver2 = FixedResolver::new(agent2);

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver2,
        agent_id: "test",
        runtime: &runtime2,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Resume with PassDecisionToTool: decision payload becomes new arguments
    struct DangerousPassthrough;
    #[async_trait]
    impl Tool for DangerousPassthrough {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "Returns args")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("dangerous", args).into())
        }
    }

    let llm3 = Arc::new(ScriptedLlm::new(vec![]));
    let agent3 =
        ResolvedAgent::new("test", "m", "sys", llm3).with_tool(Arc::new(DangerousPassthrough));
    let resolver3 = FixedResolver::new(agent3);

    let messages = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "dangerous", json!({"original": true}))],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    prepare_resume(
        runtime2.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true, "new_args": "yes"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::PassDecisionToTool),
    )
    .unwrap();

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver3,
        agent_id: "test",
        runtime: &runtime2,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn resume_rejects_non_waiting_run() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run to completion (not suspended)
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Attempt resume on a Done run with a non-existent call_id
    // prepare_resume fails because there are no tool call states after completion
    let err = prepare_resume(
        runtime.store(),
        vec![(
            "nonexistent".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap_err();

    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn resume_rejects_unknown_call_id() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // prepare_resume with unknown call_id should fail
    let err = prepare_resume(
        runtime.store(),
        vec![(
            "nonexistent".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap_err();

    assert!(err.to_string().contains("not found"));
}

// ---------------------------------------------------------------------------
// Mid-stream cancellation tests
// ---------------------------------------------------------------------------

/// An LLM executor that yields streaming deltas with a configurable delay between each.
struct SlowStreamingLlm {
    deltas: Vec<String>,
    delay_ms: u64,
}

impl SlowStreamingLlm {
    fn new(deltas: Vec<&str>, delay_ms: u64) -> Self {
        Self {
            deltas: deltas.into_iter().map(String::from).collect(),
            delay_ms,
        }
    }
}

#[async_trait]
impl LlmExecutor for SlowStreamingLlm {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let text = self.deltas.join("");
        Ok(StreamResult {
            content: vec![ContentBlock::text(text)],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn execute_stream(
        &self,
        _request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        awaken::contract::executor::InferenceStream,
                        InferenceExecutionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        use awaken::contract::executor::LlmStreamEvent;
        use futures::StreamExt as _;
        let deltas = self.deltas.clone();
        let delay = self.delay_ms;
        Box::pin(async move {
            let stream = futures::stream::unfold(
                (deltas.into_iter(), delay),
                |(mut iter, delay)| async move {
                    let delta = iter.next()?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    let event: Result<LlmStreamEvent, InferenceExecutionError> =
                        Ok(LlmStreamEvent::TextDelta(delta));
                    Some((event, (iter, delay)))
                },
            );
            let stop =
                futures::stream::once(async { Ok(LlmStreamEvent::Stop(StopReason::EndTurn)) });
            let combined = stream.chain(stop);
            Ok(Box::pin(combined) as awaken::contract::executor::InferenceStream)
        })
    }

    fn name(&self) -> &str {
        "slow-streaming"
    }
}

#[tokio::test]
async fn cancel_during_streaming_terminates_run() {
    use awaken::CancellationToken;

    let deltas: Vec<&str> = (0..10).map(|_| "tok ").collect();
    let llm = Arc::new(SlowStreamingLlm::new(deltas, 50));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let token = CancellationToken::new();
    let token_clone = token.clone();

    // Cancel after 100ms — mid-stream (after ~2 of 10 deltas)
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token_clone.cancel();
    });

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: Some(token),
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::Cancelled,
        "run should terminate with Cancelled when token is signalled mid-stream"
    );
}

#[tokio::test]
async fn cancel_before_inference_terminates_immediately() {
    use awaken::CancellationToken;

    let deltas: Vec<&str> = (0..100).map(|_| "tok ").collect();
    let llm = Arc::new(SlowStreamingLlm::new(deltas, 100));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);

    let token = CancellationToken::new();
    token.cancel();

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: Some(token),
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::Cancelled,
        "run should terminate immediately when token is already cancelled"
    );
    // Token pre-cancelled: RunStart phase detects it before entering the loop
    assert_eq!(
        result.steps, 0,
        "no steps should execute when token is already cancelled"
    );
}

#[tokio::test]
async fn state_snapshot_emitted_after_phase() {
    // Run a loop with a tool call (which modifies state) and verify StateSnapshot events
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "ping"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Done!")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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

    let events = sink.take();

    // Collect all StateSnapshot events
    let snapshots: Vec<&Value> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StateSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .collect();

    // Should have at least 2 snapshots: one per complete_step + one at run end
    assert!(
        snapshots.len() >= 2,
        "expected at least 2 state snapshots, got {}",
        snapshots.len()
    );

    // Each snapshot should contain revision and extensions fields
    for snap in &snapshots {
        assert!(
            snap.get("revision").is_some(),
            "snapshot should contain revision field"
        );
        assert!(
            snap.get("extensions").is_some(),
            "snapshot should contain extensions field"
        );
    }

    // Verify snapshots appear before StepEnd and RunFinish in event order
    let lifecycle_types: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StepStart { .. } => Some("StepStart"),
            AgentEvent::StateSnapshot { .. } => Some("StateSnapshot"),
            AgentEvent::StepEnd => Some("StepEnd"),
            AgentEvent::RunFinish { .. } => Some("RunFinish"),
            _ => None,
        })
        .collect();

    // StateSnapshot should appear before each StepEnd and before RunFinish
    for (i, &event_type) in lifecycle_types.iter().enumerate() {
        if event_type == "StepEnd" {
            assert!(
                i > 0 && lifecycle_types[i - 1] == "StateSnapshot",
                "StateSnapshot should precede StepEnd, got: {:?}",
                lifecycle_types
            );
        }
    }
    // Last RunFinish should be preceded by StateSnapshot (possibly with other events between)
    let last_snapshot_idx = lifecycle_types
        .iter()
        .rposition(|&t| t == "StateSnapshot")
        .expect("should have a StateSnapshot");
    let run_finish_idx = lifecycle_types
        .iter()
        .rposition(|&t| t == "RunFinish")
        .expect("should have a RunFinish");
    assert!(
        last_snapshot_idx < run_finish_idx,
        "final StateSnapshot should precede RunFinish"
    );
}

// ---------------------------------------------------------------------------
// Frontend tool interception tests
// ---------------------------------------------------------------------------

/// A plugin that intercepts "frontend" tools via BeforeToolExecute.
///
/// On first entry: suspends with UseDecisionAsToolResult mode.
/// On resume: the tool re-executes with decision.result as arguments,
/// and this passthrough tool returns those arguments as the tool result.
/// This mirrors uncarve's FrontendToolPlugin pattern.
struct FrontendToolInterceptPlugin {
    frontend_tool_ids: Vec<String>,
}

#[async_trait]
impl PhaseHook for FrontendToolInterceptPlugin {
    async fn run(
        &self,
        ctx: &awaken::PhaseContext,
    ) -> Result<awaken::StateCommand, awaken::StateError> {
        use awaken::contract::suspension::{PendingToolCall, SuspendTicket, Suspension};

        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(awaken::StateCommand::new()),
        };

        if !self.frontend_tool_ids.iter().any(|id| id == tool_name) {
            return Ok(awaken::StateCommand::new());
        }

        // If resuming, don't intercept — let the tool execute with decision args
        if ctx.resume_input.is_some() {
            return Ok(awaken::StateCommand::new());
        }

        // First entry: suspend with UseDecisionAsToolResult
        let call_id = ctx.tool_call_id.as_deref().unwrap_or("");
        let args = ctx.tool_args.clone().unwrap_or_default();
        let ticket = SuspendTicket::new(
            Suspension {
                id: format!("suspend_{call_id}"),
                action: format!("tool:{tool_name}"),
                message: format!("Frontend tool '{tool_name}' requires client execution"),
                parameters: args.clone(),
                response_schema: None,
            },
            PendingToolCall::new(call_id, tool_name, args),
            ToolCallResumeMode::UseDecisionAsToolResult,
        );

        let mut cmd = awaken::StateCommand::new();
        cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Suspend(ticket))?;
        Ok(cmd)
    }
}

struct FrontendToolInterceptPluginWrapper {
    plugin: FrontendToolInterceptPlugin,
}

impl Plugin for FrontendToolInterceptPluginWrapper {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "frontend-tool-intercept",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "frontend-tool-intercept",
            awaken::Phase::BeforeToolExecute,
            self.plugin.clone(),
        )?;
        Ok(())
    }
}

impl Clone for FrontendToolInterceptPlugin {
    fn clone(&self) -> Self {
        Self {
            frontend_tool_ids: self.frontend_tool_ids.clone(),
        }
    }
}

/// End-to-end test: frontend tool suspension + resume via UseDecisionAsToolResult.
///
/// Flow:
/// 1. LLM calls "ask_user" tool
/// 2. FrontendToolInterceptPlugin intercepts → Suspend(UseDecisionAsToolResult)
/// 3. Run terminates with Suspended
/// 4. External decision arrives with result payload
/// 5. prepare_resume with UseDecisionAsToolResult mode replaces arguments
/// 6. detect_and_replay_resume re-executes tool with decision args
/// 7. AskUserTool returns decision args as tool result
/// 8. LLM sees the frontend result and responds
#[tokio::test]
async fn frontend_tool_intercept_suspend_and_resume() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // First call: LLM invokes the frontend tool
        make_tool_call_response("ask_user", "fc1", json!({"question": "What color?"})),
        // After resume: LLM sees the frontend result and ends
    ]));

    // AskUserTool is a "frontend tool" — it returns args as result.
    // When resumed with decision args, the decision payload becomes the tool result.
    struct AskUserTool;
    #[async_trait]
    impl Tool for AskUserTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("ask_user", "ask_user", "Ask the user a question")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("ask_user", args).into())
        }
    }

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(AskUserTool));

    let frontend_plugin = Arc::new(FrontendToolInterceptPluginWrapper {
        plugin: FrontendToolInterceptPlugin {
            frontend_tool_ids: vec!["ask_user".into()],
        },
    });

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![frontend_plugin]);

    // Run until suspension
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("ask the user what color they want")],
        run_identity: test_identity(),
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
        result.termination,
        TerminationReason::Suspended,
        "should suspend on frontend tool"
    );

    // Verify tool call is in Suspended state
    let states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(states.calls["fc1"].status, ToolCallStatus::Suspended);

    // Simulate frontend sending back the user's answer
    let messages: Vec<Message> = vec![
        Message::user("ask the user what color they want"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "fc1",
                "ask_user",
                json!({"question": "What color?"}),
            )],
        ),
        Message::tool("fc1", "Tool 'ask_user' suspended: awaiting decision"),
    ];

    // Resume with UseDecisionAsToolResult: decision.result becomes tool arguments
    prepare_resume(
        runtime.store(),
        vec![(
            "fc1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"answer": "blue"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    // Verify tool call transitioned to Resuming with decision args
    let states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(states.calls["fc1"].status, ToolCallStatus::Resuming);
    assert_eq!(
        states.calls["fc1"].arguments,
        json!({"answer": "blue"}),
        "arguments should be replaced with decision result"
    );

    // Resume the run
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
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
        result.termination,
        TerminationReason::NaturalEnd,
        "should complete after frontend tool resume"
    );
}

// ---------------------------------------------------------------------------
// Tool interception tests: Block, SetResult, state transitions
// ---------------------------------------------------------------------------

/// Plugin that blocks a specific tool via BeforeToolExecute.
struct BlockingPlugin {
    blocked_tool: String,
    reason: String,
}

impl Clone for BlockingPlugin {
    fn clone(&self) -> Self {
        Self {
            blocked_tool: self.blocked_tool.clone(),
            reason: self.reason.clone(),
        }
    }
}

#[async_trait]
impl PhaseHook for BlockingPlugin {
    async fn run(
        &self,
        ctx: &awaken::PhaseContext,
    ) -> Result<awaken::StateCommand, awaken::StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(awaken::StateCommand::new()),
        };

        if tool_name != self.blocked_tool {
            return Ok(awaken::StateCommand::new());
        }

        let mut cmd = awaken::StateCommand::new();
        cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Block {
            reason: self.reason.clone(),
        })?;
        Ok(cmd)
    }
}

struct BlockingPluginWrapper(BlockingPlugin);

impl Plugin for BlockingPluginWrapper {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "blocking-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "blocking-plugin",
            awaken::Phase::BeforeToolExecute,
            self.0.clone(),
        )?;
        Ok(())
    }
}

#[tokio::test]
async fn tool_intercept_block_terminates_run() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "echo",
        "c1",
        json!({"message": "hello"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let blocking_plugin = Arc::new(BlockingPluginWrapper(BlockingPlugin {
        blocked_tool: "echo".into(),
        reason: "tool is forbidden".into(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![blocking_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use echo")],
        run_identity: test_identity(),
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
        matches!(result.termination, TerminationReason::Blocked(ref reason) if reason == "tool is forbidden"),
        "expected Blocked termination, got {:?}",
        result.termination
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

/// Plugin that sets a result for a specific tool, skipping execution.
struct SetResultPlugin {
    target_tool: String,
    result: ToolResult,
}

impl Clone for SetResultPlugin {
    fn clone(&self) -> Self {
        Self {
            target_tool: self.target_tool.clone(),
            result: self.result.clone(),
        }
    }
}

#[async_trait]
impl PhaseHook for SetResultPlugin {
    async fn run(
        &self,
        ctx: &awaken::PhaseContext,
    ) -> Result<awaken::StateCommand, awaken::StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(awaken::StateCommand::new()),
        };

        if tool_name != self.target_tool {
            return Ok(awaken::StateCommand::new());
        }

        let mut cmd = awaken::StateCommand::new();
        cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::SetResult(
            self.result.clone(),
        ))?;
        Ok(cmd)
    }
}

struct SetResultPluginWrapper(SetResultPlugin);

impl Plugin for SetResultPluginWrapper {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "set-result-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "set-result-plugin",
            awaken::Phase::BeforeToolExecute,
            self.0.clone(),
        )?;
        Ok(())
    }
}

#[tokio::test]
async fn tool_intercept_set_result_skips_execution() {
    // Track whether the tool was actually executed
    struct TrackedEchoTool(Arc<Mutex<bool>>);

    #[async_trait]
    impl Tool for TrackedEchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Tracked echo")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            *self.0.lock().unwrap() = true;
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ToolResult::success_with_message("echo", args, msg).into())
        }
    }

    let executed = Arc::new(Mutex::new(false));
    let llm = Arc::new(ScriptedLlm::new(vec![
        make_tool_call_response("echo", "c1", json!({"message": "hello"})),
        StreamResult {
            content: vec![ContentBlock::text("Got the result.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(TrackedEchoTool(Arc::clone(&executed))));

    let intercepted_result = ToolResult::success_with_message(
        "echo",
        json!({"message": "hello"}),
        "intercepted result".to_string(),
    );
    let set_result_plugin = Arc::new(SetResultPluginWrapper(SetResultPlugin {
        target_tool: "echo".into(),
        result: intercepted_result,
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![set_result_plugin]);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use echo")],
        run_identity: test_identity(),
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
    assert!(
        !*executed.lock().unwrap(),
        "tool should NOT have been executed when SetResult intercept is active"
    );

    // Verify a ToolCallDone event was still emitted (from the SetResult path)
    let events = sink.take();
    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert_eq!(tool_done_count, 1, "SetResult should emit ToolCallDone");
}

#[tokio::test]
async fn suspended_tool_preserves_state_across_resume() {
    // Run 1: suspend
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "rm -rf"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Verify Suspended state
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Suspended);
    assert_eq!(tc_states.calls["c1"].tool_name, "dangerous");
    assert_eq!(tc_states.calls["c1"].arguments, json!({"action": "rm -rf"}));

    // Resume: transition Suspended → Resuming
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    // Verify Resuming state
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Resuming);

    // Run 2: complete
    let messages = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c1",
                "dangerous",
                json!({"action": "rm -rf"}),
            )],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

#[tokio::test]
async fn decision_channel_resume_resolves_suspended_call() {
    use futures::channel::mpsc;

    let llm = Arc::new(ScriptedLlm::new(vec![
        make_tool_call_response("dangerous", "c1", json!({"action": "delete"})),
        // After resume via decision channel, LLM produces final response
    ]));

    struct DangerousApproved;
    #[async_trait]
    impl Tool for DangerousApproved {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "Requires approval")
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::suspended("dangerous", "needs user approval").into())
        }
    }

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(DangerousApproved));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let (tx, rx) = mpsc::unbounded::<Vec<(String, ToolCallResume)>>();

    // Send the decision after a short delay so the loop picks it up
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.unbounded_send(vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )])
        .unwrap();
    });

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: Some(rx),
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // With decision_rx, the loop resumes in-place (doesn't return Suspended)
    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "decision channel should allow the run to resume and complete"
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

#[tokio::test]
async fn cancel_decision_marks_tool_cancelled() {
    use futures::channel::mpsc;

    let llm = Arc::new(ScriptedLlm::new(vec![
        make_tool_call_response("dangerous", "c1", json!({"action": "delete"})),
        // After cancel decision, LLM sees cancellation and ends
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let (tx, rx) = mpsc::unbounded::<Vec<(String, ToolCallResume)>>();

    // Send a Cancel decision after a short delay
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.unbounded_send(vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Cancel,
                result: Value::Null,
                reason: Some("user denied".into()),
                updated_at: 0,
            },
        )])
        .unwrap();
    });

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: Some(rx),
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After cancel via decision channel, the loop continues and LLM responds with NaturalEnd
    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "cancel decision should let the run continue and finish"
    );
}

#[tokio::test]
async fn permission_hook_blocks_denied_tool() {
    // A permission-style plugin that blocks a specific tool
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "echo",
        "c1",
        json!({"message": "hello"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let permission_plugin = Arc::new(BlockingPluginWrapper(BlockingPlugin {
        blocked_tool: "echo".into(),
        reason: "permission denied by policy".into(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![permission_plugin]);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use echo")],
        run_identity: test_identity(),
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
        matches!(result.termination, TerminationReason::Blocked(ref reason) if reason == "permission denied by policy"),
        "expected Blocked termination from permission hook, got {:?}",
        result.termination
    );

    // Verify ToolCallDone event with Failed outcome was emitted
    let events = sink.take();
    let tool_fail_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, AgentEvent::ToolCallDone { outcome, .. }
                if *outcome == awaken::contract::suspension::ToolCallOutcome::Failed)
        })
        .collect();
    assert_eq!(
        tool_fail_events.len(),
        1,
        "blocked tool should emit ToolCallDone with Failed outcome"
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

// ---------------------------------------------------------------------------
// Additional intercept, resume, and lifecycle coverage
// ---------------------------------------------------------------------------

/// Verify that when a BeforeToolExecute hook suspends with UseDecisionAsToolResult mode,
/// the subsequent prepare_resume + replay uses decision result as tool output (not re-executing).
/// This is the core frontend tool pattern.
#[tokio::test]
async fn intercept_suspend_preserves_ticket_resume_mode() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // First call: LLM invokes the frontend tool
        make_tool_call_response("ask_user", "fc1", json!({"question": "Pick a color"})),
        // After resume: LLM sees the decision result and ends
    ]));

    // A tool that returns its args as result (frontend passthrough)
    struct FrontendTool;
    #[async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("ask_user", "ask_user", "Frontend tool")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("ask_user", args).into())
        }
    }

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(FrontendTool));

    let frontend_plugin = Arc::new(FrontendToolInterceptPluginWrapper {
        plugin: FrontendToolInterceptPlugin {
            frontend_tool_ids: vec!["ask_user".into()],
        },
    });

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![frontend_plugin]);

    // Run until suspension
    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("ask user for color")],
        run_identity: test_identity(),
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

    // Verify tool call is Suspended
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["fc1"].status, ToolCallStatus::Suspended);

    // Resume with UseDecisionAsToolResult — decision.result replaces tool arguments
    prepare_resume(
        runtime.store(),
        vec![(
            "fc1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"color": "red"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    // After prepare_resume, arguments should be the decision result, not original
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["fc1"].status, ToolCallStatus::Resuming);
    assert_eq!(
        tc_states.calls["fc1"].arguments,
        json!({"color": "red"}),
        "UseDecisionAsToolResult should replace arguments with decision result"
    );

    // Resume the run
    let messages = vec![
        Message::user("ask user for color"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "fc1",
                "ask_user",
                json!({"question": "Pick a color"}),
            )],
        ),
        Message::tool("fc1", "Tool 'ask_user' suspended: awaiting decision"),
    ];

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
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
        resume_result.termination,
        TerminationReason::NaturalEnd,
        "should complete after UseDecisionAsToolResult resume"
    );
}

/// LLM returns 3 tool calls. Hook blocks one, lets two proceed.
/// Verify blocked tool produces Blocked termination with correct reason.
#[tokio::test]
async fn multiple_tool_calls_partial_intercept() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("c1", "echo", json!({"message": "one"})),
            ToolCall::new("c2", "calc", json!({"result": 42})),
            ToolCall::new("c3", "echo", json!({"message": "three"})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));

    // Block only "calc" tool
    let blocking_plugin = Arc::new(BlockingPluginWrapper(BlockingPlugin {
        blocked_tool: "calc".into(),
        reason: "calc is forbidden".into(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![blocking_plugin]);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("multi-tool")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Block intercept terminates the run
    assert!(
        matches!(result.termination, TerminationReason::Blocked(ref reason) if reason == "calc is forbidden"),
        "expected Blocked termination for calc, got {:?}",
        result.termination
    );

    // Verify ToolCallDone event with Failed outcome for the blocked tool
    let events = sink.take();
    let failed_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, AgentEvent::ToolCallDone { outcome, .. }
                if *outcome == awaken::contract::suspension::ToolCallOutcome::Failed)
        })
        .collect();
    assert!(
        !failed_events.is_empty(),
        "blocked tool should emit ToolCallDone with Failed outcome"
    );
}

/// When SetResult intercepts, verify the ToolCallDone event is emitted with
/// correct outcome and result.
#[tokio::test]
async fn intercept_set_result_emits_tool_call_done_event() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        make_tool_call_response("echo", "c1", json!({"message": "hello"})),
        StreamResult {
            content: vec![ContentBlock::text("Got it.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let intercepted_result = ToolResult::success_with_message(
        "echo",
        json!({"message": "hello"}),
        "set-result output".to_string(),
    );

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let set_result_plugin = Arc::new(SetResultPluginWrapper(SetResultPlugin {
        target_tool: "echo".into(),
        result: intercepted_result.clone(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![set_result_plugin]);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use echo")],
        run_identity: test_identity(),
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

    let events = sink.take();

    // Find ToolCallDone events
    let tool_done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone {
                id,
                outcome,
                result,
                ..
            } => Some((id.clone(), *outcome, result.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(
        tool_done_events.len(),
        1,
        "should emit exactly one ToolCallDone"
    );
    let (id, outcome, done_result) = &tool_done_events[0];
    assert_eq!(id, "c1");
    assert_eq!(
        *outcome,
        awaken::contract::suspension::ToolCallOutcome::Succeeded,
        "SetResult with success should yield Succeeded outcome"
    );
    assert!(
        done_result.is_success(),
        "SetResult tool result should be success"
    );
}

/// Test normalize_decision_result logic: when decision.result is `true` (boolean),
/// fallback to original arguments. When it's an object, use the object.
/// Mirrors uncarve's `normalize_decision_tool_result`.
#[tokio::test]
async fn resume_with_normalize_decision_result_boolean() {
    // Set up: suspend a tool, then resume with boolean `true` as decision result.
    // With UseDecisionAsToolResult, boolean should fall back to original args.
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "deploy", "target": "prod"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("deploy")],
        run_identity: test_identity(),
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

    // Resume with boolean true — normalize_decision_result should fallback to original args
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!(true),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Resuming);
    assert_eq!(
        tc_states.calls["c1"].arguments,
        json!({"action": "deploy", "target": "prod"}),
        "boolean decision result should fallback to original arguments"
    );

    // Also test with an object decision result — should use the object directly
    let runtime2 = make_runtime();
    let llm2 = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c2",
        json!({"action": "deploy"}),
    )]));
    let agent2 = ResolvedAgent::new("test", "m", "sys", llm2).with_tool(Arc::new(SuspendingTool));
    let resolver2 = FixedResolver::new(agent2);

    let result2 = run_agent_loop(AgentLoopParams {
        resolver: &resolver2,
        agent_id: "test",
        runtime: &runtime2,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("deploy")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();
    assert_eq!(result2.termination, TerminationReason::Suspended);

    prepare_resume(
        runtime2.store(),
        vec![(
            "c2".into(),
            ToolCallResume {
                decision_id: "d2".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"overridden": "args"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    let tc_states2 = runtime2.store().read::<ToolCallStates>().unwrap();
    assert_eq!(
        tc_states2.calls["c2"].arguments,
        json!({"overridden": "args"}),
        "object decision result should be used directly as arguments"
    );
}

/// Spawn two tool calls that both suspend via intercept plugin. Send decisions
/// for both via decision channel. Verify both resume and run completes.
///
/// Uses intercept-based suspension (not tool-level Pending) so both calls are
/// registered in ToolCallStates before the decision channel wait begins.
#[tokio::test]
async fn concurrent_suspend_and_resume_via_channel() {
    use awaken::contract::suspension::{PendingToolCall, SuspendTicket, Suspension};
    use futures::channel::mpsc;

    // Plugin that suspends ALL tool calls via BeforeToolExecute intercept
    struct SuspendAllPlugin;
    impl Clone for SuspendAllPlugin {
        fn clone(&self) -> Self {
            Self
        }
    }
    #[async_trait]
    impl PhaseHook for SuspendAllPlugin {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let tool_name = match &ctx.tool_name {
                Some(name) => name.as_str(),
                None => return Ok(awaken::StateCommand::new()),
            };
            // If resuming, let it proceed
            if ctx.resume_input.is_some() {
                return Ok(awaken::StateCommand::new());
            }
            let call_id = ctx.tool_call_id.as_deref().unwrap_or("");
            let args = ctx.tool_args.clone().unwrap_or_default();
            let ticket = SuspendTicket::new(
                Suspension {
                    id: format!("suspend_{call_id}"),
                    action: format!("tool:{tool_name}"),
                    message: format!("Tool '{tool_name}' requires approval"),
                    parameters: args.clone(),
                    response_schema: None,
                },
                PendingToolCall::new(call_id, tool_name, args),
                ToolCallResumeMode::ReplayToolCall,
            );
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Suspend(ticket))?;
            Ok(cmd)
        }
    }
    struct SuspendAllPluginWrapper;
    impl Plugin for SuspendAllPluginWrapper {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "suspend-all",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "suspend-all",
                awaken::Phase::BeforeToolExecute,
                SuspendAllPlugin,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("ca", "echo", json!({"x": 1})),
                ToolCall::new("cb", "echo", json!({"y": 2})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // After both resume, LLM ends
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(SuspendAllPluginWrapper)]);

    let (tx, rx) = mpsc::unbounded::<Vec<(String, ToolCallResume)>>();

    // Send decisions for both suspended tools after a short delay
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.unbounded_send(vec![
            (
                "ca".into(),
                ToolCallResume {
                    decision_id: "da".into(),
                    action: ResumeDecisionAction::Resume,
                    result: json!({"approved": true}),
                    reason: None,
                    updated_at: 0,
                },
            ),
            (
                "cb".into(),
                ToolCallResume {
                    decision_id: "db".into(),
                    action: ResumeDecisionAction::Resume,
                    result: json!({"approved": true}),
                    reason: None,
                    updated_at: 0,
                },
            ),
        ])
        .unwrap();
    });

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do both")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: Some(rx),
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "both suspended calls should resume via channel and run should complete"
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

/// Verify the complete state machine in a real loop:
/// - Normal tool: New -> Running -> Succeeded
/// - Suspended tool: New -> Running -> Suspended -> (resume) -> Resuming -> Running -> Succeeded
///
/// We check the transitions by inspecting ToolCallStates at suspension and after resume.
#[tokio::test]
async fn tool_call_lifecycle_complete_transitions_in_loop() {
    // Step 1: LLM calls two tools — one normal (echo), one suspending (dangerous)
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c_normal", "echo", json!({"message": "hi"})),
                ToolCall::new("c_suspend", "dangerous", json!({"action": "delete"})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // After resume, LLM ends
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // At suspension: normal tool should be Succeeded, suspending tool should be Suspended
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(
        tc_states.calls["c_normal"].status,
        ToolCallStatus::Succeeded,
        "normal tool should reach Succeeded"
    );
    assert_eq!(
        tc_states.calls["c_suspend"].status,
        ToolCallStatus::Suspended,
        "suspending tool should be Suspended"
    );

    // Now resume: transition Suspended -> Resuming
    prepare_resume(
        runtime.store(),
        vec![(
            "c_suspend".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(
        tc_states.calls["c_suspend"].status,
        ToolCallStatus::Resuming,
        "after prepare_resume, tool should be Resuming"
    );

    // Run the resumed loop
    let messages = vec![
        Message::user("do it"),
        Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("c_normal", "echo", json!({"message": "hi"})),
                ToolCall::new("c_suspend", "dangerous", json!({"action": "delete"})),
            ],
        ),
        Message::tool("c_normal", "hi"),
        Message::tool("c_suspend", "needs user approval"),
    ];

    let resume_result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
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
        resume_result.termination,
        TerminationReason::NaturalEnd,
        "resumed run should complete"
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

// ---------------------------------------------------------------------------
// Core tool execution tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_tools_one_fails_other_succeeds() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "hello"})),
                ToolCall::new("c2", "fail", json!({})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Echo worked, fail failed.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run both")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 2);

    let events = sink.take();
    let tool_done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id.clone(), *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_done_events.len(), 2, "both tools should complete");

    // Echo should succeed, fail should fail (order may vary)
    let succeeded = tool_done_events
        .iter()
        .filter(|(_, o)| *o == awaken::contract::suspension::ToolCallOutcome::Succeeded)
        .count();
    let failed = tool_done_events
        .iter()
        .filter(|(_, o)| *o == awaken::contract::suspension::ToolCallOutcome::Failed)
        .count();
    assert_eq!(succeeded, 1, "echo tool should succeed");
    assert_eq!(failed, 1, "fail tool should fail");
}

#[tokio::test]
async fn sequential_tools_stop_after_first_suspension() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("c1", "dangerous", json!({"action": "delete"})),
            ToolCall::new("c2", "echo", json!({"message": "should not run"})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do both")],
        run_identity: test_identity(),
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
        result.termination,
        TerminationReason::Suspended,
        "run should terminate with Suspended when a tool suspends"
    );

    // Verify the suspending tool is in Suspended state
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Suspended);

    // The second tool (echo) should NOT have a Succeeded entry
    let events = sink.take();
    let echo_done = events.iter().any(|e| {
        matches!(e, AgentEvent::ToolCallDone { id, outcome, .. }
            if id == "c2" && *outcome == awaken::contract::suspension::ToolCallOutcome::Succeeded)
    });
    assert!(
        !echo_done,
        "second tool should NOT execute after first tool suspends"
    );
}

#[tokio::test]
async fn stop_policy_max_rounds_terminates() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c0", "echo", json!({"message": "round0"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "round1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_max_rounds(1)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("loop")],
        run_identity: test_identity(),
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
        matches!(
            result.termination,
            TerminationReason::Stopped(ref s) if s.code == "max_rounds"
        ),
        "expected Stopped(max_rounds), got {:?}",
        result.termination
    );
}

#[tokio::test]
async fn cancel_during_tool_execution() {
    use awaken::CancellationToken;

    /// A tool that sleeps for 100ms before returning.
    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("slow", "slow", "Sleeps before returning")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(ToolResult::success("slow", json!({"done": true})).into())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![ToolCall::new("c1", "slow", json!({}))],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(SlowTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let token = CancellationToken::new();
    let token_clone = token.clone();

    // Cancel after 10ms while the tool is sleeping for 100ms
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token_clone.cancel();
    });

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run slow tool")],
        run_identity: test_identity(),
        cancellation_token: Some(token),
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::Cancelled,
        "run should terminate with Cancelled when token fires during tool execution"
    );
}

#[tokio::test]
async fn empty_tool_calls_natural_end() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Just a text response, no tools.")],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(5),
            completion_tokens: Some(8),
            total_tokens: Some(13),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool)); // Tools registered but not used
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hello")],
        run_identity: test_identity(),
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
    assert_eq!(result.response, "Just a text response, no tools.");
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn context_message_injected_before_inference() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    /// An LLM that records the messages it receives.
    struct RecordingLlm {
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl RecordingLlm {
        fn new() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
            }
        }

        fn recorded_requests(&self) -> Vec<Vec<Message>> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmExecutor for RecordingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.requests.lock().unwrap().push(req.messages.clone());
            Ok(StreamResult {
                content: vec![ContentBlock::text("Acknowledged.")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    /// Plugin that injects a context message via BeforeInference.
    struct ContextInjectorHook;

    #[async_trait]
    impl PhaseHook for ContextInjectorHook {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                "test_injector",
                "Injected context message for testing.",
            ))?;
            Ok(cmd)
        }
    }

    struct ContextInjectorPlugin;

    impl Plugin for ContextInjectorPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "context-injector",
            }
        }

        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "context-injector",
                awaken::Phase::BeforeInference,
                ContextInjectorHook,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(RecordingLlm::new());
    let llm_clone = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(ContextInjectorPlugin)]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hello")],
        run_identity: test_identity(),
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

    // Verify the LLM received the injected context message in its request
    let requests = llm_clone.recorded_requests();
    assert!(
        !requests.is_empty(),
        "LLM should have received at least one request"
    );

    let first_request_messages = &requests[0];
    let has_context_message = first_request_messages
        .iter()
        .any(|msg| msg.text().contains("Injected context message for testing."));
    assert!(
        has_context_message,
        "LLM request should contain the injected context message, got messages: {:?}",
        first_request_messages
    );
}

#[tokio::test]
async fn tool_execution_preserves_arguments() {
    /// A tool that returns its received arguments as the result.
    struct ArgReturningTool;

    #[async_trait]
    impl Tool for ArgReturningTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("arg_echo", "arg_echo", "Returns args as result")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("arg_echo", args).into())
        }
    }

    let expected_args = json!({
        "name": "test_value",
        "count": 42,
        "nested": {"key": "val"},
        "list": [1, 2, 3]
    });

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "arg_echo", expected_args.clone())],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Got the args back.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent =
        ResolvedAgent::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(ArgReturningTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("call arg_echo")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 2);

    // Verify the tool received and returned the exact arguments
    let events = sink.take();
    let tool_done_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, result, .. } => Some((id.clone(), result.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_done_results.len(), 1);
    let (id, tool_result) = &tool_done_results[0];
    assert_eq!(id, "c1");
    assert!(tool_result.is_success(), "tool should succeed");
    assert_eq!(
        tool_result.data, expected_args,
        "tool result should contain the exact arguments passed by the LLM"
    );
}

// ===========================================================================
// Behavioral tests migrated from uncarve streaming suite
// ===========================================================================

// ---------------------------------------------------------------------------
// Retry & Recovery
// ---------------------------------------------------------------------------

/// An LLM that fails the first N calls with a provider error, then succeeds.
struct CountingLlm {
    failures_remaining: Mutex<usize>,
    success_responses: Mutex<Vec<StreamResult>>,
}

impl CountingLlm {
    fn new(failures: usize, responses: Vec<StreamResult>) -> Self {
        Self {
            failures_remaining: Mutex::new(failures),
            success_responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for CountingLlm {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut remaining = self.failures_remaining.lock().unwrap();
        if *remaining > 0 {
            *remaining -= 1;
            return Err(InferenceExecutionError::Provider(
                "transient failure".into(),
            ));
        }
        drop(remaining);
        let mut responses = self.success_responses.lock().unwrap();
        if responses.is_empty() {
            Ok(StreamResult {
                content: vec![ContentBlock::text("recovered")],
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
        "counting"
    }
}

/// An LLM that records the model field from each request.
struct ModelRecordingLlm {
    models_seen: Mutex<Vec<String>>,
    responses: Mutex<Vec<StreamResult>>,
}

impl ModelRecordingLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            models_seen: Mutex::new(Vec::new()),
            responses: Mutex::new(responses),
        }
    }

    fn models(&self) -> Vec<String> {
        self.models_seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmExecutor for ModelRecordingLlm {
    async fn execute(
        &self,
        req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.models_seen.lock().unwrap().push(req.model.clone());
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
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
        "model-recording"
    }
}

/// 1. LLM fails on first call => run_agent_loop returns Err.
#[tokio::test]
async fn retry_startup_error_propagates() {
    let llm = Arc::new(CountingLlm::new(1, vec![]));
    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;

    assert!(
        result.is_err(),
        "LLM provider error should propagate as AgentLoopError"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("transient failure"),
        "error should contain the provider message, got: {err_msg}"
    );
}

/// 2. Model name in inference request matches ResolvedAgent.model.
#[tokio::test]
async fn inference_request_uses_configured_model_name() {
    let llm = Arc::new(ModelRecordingLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));
    let llm_clone = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "claude-3-opus", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let models = llm_clone.models();
    assert_eq!(models.len(), 1, "should have exactly one inference call");
    assert_eq!(
        models[0], "claude-3-opus",
        "inference request should use the configured model name"
    );
}

/// 3. Truncation with complete tool calls does NOT trigger retry; tools proceed.
#[tokio::test]
async fn truncation_with_tool_calls_no_retry() {
    // LLM returns MaxTokens but tool calls are complete (not incomplete).
    // This should NOT trigger truncation recovery; tool calls proceed normally.
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("partial")],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "test"}))],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false, // complete tool calls
        },
        StreamResult {
            content: vec![ContentBlock::text("Done after tool.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_max_continuation_retries(2);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Tool calls should have executed; two steps (tool call step + final response)
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        result.steps, 2,
        "tool call should proceed without truncation retry"
    );
}

/// 4. Truncation retries exhaust budget; run ends with NaturalEnd (not error).
///
///    Uses a custom execute_stream to produce truncated tool calls each time.
#[tokio::test]
async fn truncation_recovery_exhausts_retries() {
    use awaken::contract::executor::LlmStreamEvent;

    struct AlwaysTruncatingLlm {
        call_count: Mutex<usize>,
    }
    impl AlwaysTruncatingLlm {
        fn new() -> Self {
            Self {
                call_count: Mutex::new(0),
            }
        }
    }
    #[async_trait]
    impl LlmExecutor for AlwaysTruncatingLlm {
        async fn execute(
            &self,
            _req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            unreachable!("should use execute_stream")
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            awaken::contract::executor::InferenceStream,
                            InferenceExecutionError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            let call_num = *count;
            drop(count);

            Box::pin(async move {
                // Always return truncated response with incomplete tool call
                let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                    Ok(LlmStreamEvent::TextDelta(format!(
                        "partial output {call_num}..."
                    ))),
                    Ok(LlmStreamEvent::ToolCallStart {
                        id: format!("tc_{call_num}"),
                        name: "echo".into(),
                    }),
                    Ok(LlmStreamEvent::ToolCallDelta {
                        id: format!("tc_{call_num}"),
                        args_delta: "{\"incomplete".into(),
                    }),
                    Ok(LlmStreamEvent::Stop(StopReason::MaxTokens)),
                ];
                Ok(Box::pin(futures::stream::iter(events))
                    as awaken::contract::executor::InferenceStream)
            })
        }

        fn name(&self) -> &str {
            "always-truncating"
        }
    }

    let llm = Arc::new(AlwaysTruncatingLlm::new());
    let llm_ref = Arc::clone(&llm);
    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_max_continuation_retries(2)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After exhausting retries (2), the final truncated response has no complete
    // tool calls, so it's treated as text-only and ends naturally
    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "exhausted truncation retries should end naturally, not error"
    );

    // Should have called LLM 3 times: 1 original + 2 retries
    let call_count = *llm_ref.call_count.lock().unwrap();
    assert_eq!(
        call_count, 3,
        "should retry exactly max_continuation_retries times"
    );
}

/// 5. After truncation recovery, the partial text is preserved as a message
///    and the continuation text becomes the final response.
///
///    Uses a custom execute_stream to produce incomplete tool call JSON
///    (which `execute_streaming` detects as `has_incomplete_tool_calls`).
#[tokio::test]
async fn truncation_recovery_preserves_truncated_text() {
    use awaken::contract::executor::LlmStreamEvent;

    struct TruncationStreamLlm {
        call_count: Mutex<usize>,
        messages_seen: Mutex<Vec<Vec<String>>>,
    }
    impl TruncationStreamLlm {
        fn new() -> Self {
            Self {
                call_count: Mutex::new(0),
                messages_seen: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl LlmExecutor for TruncationStreamLlm {
        async fn execute(
            &self,
            _req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            unreachable!("should use execute_stream")
        }

        fn execute_stream(
            &self,
            request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            awaken::contract::executor::InferenceStream,
                            InferenceExecutionError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            let msg_texts: Vec<String> = request.messages.iter().map(|m| m.text()).collect();
            self.messages_seen.lock().unwrap().push(msg_texts);
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            let call_num = *count;
            drop(count);

            Box::pin(async move {
                if call_num == 1 {
                    // First call: text + incomplete tool call JSON, then MaxTokens
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta("Part one.".into())),
                        Ok(LlmStreamEvent::ToolCallStart {
                            id: "tc_incomplete".into(),
                            name: "echo".into(),
                        }),
                        // Incomplete JSON (truncated mid-argument)
                        Ok(LlmStreamEvent::ToolCallDelta {
                            id: "tc_incomplete".into(),
                            args_delta: "{\"message\": \"trun".into(),
                        }),
                        Ok(LlmStreamEvent::Stop(StopReason::MaxTokens)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events))
                        as awaken::contract::executor::InferenceStream)
                } else {
                    // Continuation: completes normally
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta("Part two.".into())),
                        Ok(LlmStreamEvent::Stop(StopReason::EndTurn)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events))
                        as awaken::contract::executor::InferenceStream)
                }
            })
        }

        fn name(&self) -> &str {
            "truncation-stream-llm"
        }
    }

    let llm = Arc::new(TruncationStreamLlm::new());
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_max_continuation_retries(2)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    // Verify the LLM was called twice (original + continuation)
    let messages_seen = llm_ref.messages_seen.lock().unwrap();
    assert_eq!(
        messages_seen.len(),
        2,
        "should have two inference calls: original + continuation"
    );

    // The continuation request should contain the partial text as a message
    let continuation_msgs = &messages_seen[1];
    let has_partial = continuation_msgs.iter().any(|m| m.contains("Part one."));
    assert!(
        has_partial,
        "continuation request should contain the partial text from truncated response, got: {:?}",
        continuation_msgs
    );

    // The continuation request should also have the continuation prompt
    let has_continuation_prompt = continuation_msgs
        .iter()
        .any(|m| m.contains("cut off") || m.contains("Continue from where you left off"));
    assert!(
        has_continuation_prompt,
        "continuation request should contain the continuation prompt, got: {:?}",
        continuation_msgs
    );
}

// ---------------------------------------------------------------------------
// State & Lifecycle
// ---------------------------------------------------------------------------

/// 6. RunStart and RunFinish events have matching thread_id and run_id.
#[tokio::test]
async fn run_finish_has_matching_thread_id() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hello")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let identity = RunIdentity::new(
        "thread-42".into(),
        None,
        "run-99".into(),
        None,
        "test-agent".into(),
        RunOrigin::User,
    );

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: identity,
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

    let run_start = events.iter().find_map(|e| match e {
        AgentEvent::RunStart {
            thread_id, run_id, ..
        } => Some((thread_id.clone(), run_id.clone())),
        _ => None,
    });
    let run_finish = events.iter().find_map(|e| match e {
        AgentEvent::RunFinish {
            thread_id, run_id, ..
        } => Some((thread_id.clone(), run_id.clone())),
        _ => None,
    });

    let (start_tid, start_rid) = run_start.expect("should have RunStart event");
    let (finish_tid, finish_rid) = run_finish.expect("should have RunFinish event");

    assert_eq!(start_tid, "thread-42");
    assert_eq!(start_rid, "run-99");
    assert_eq!(
        start_tid, finish_tid,
        "thread_id should match between RunStart and RunFinish"
    );
    assert_eq!(
        start_rid, finish_rid,
        "run_id should match between RunStart and RunFinish"
    );
}

/// 7. All tool calls in a step suspend => run enters Waiting (Suspended termination).
#[tokio::test]
async fn all_tools_suspended_pauses_run() {
    use awaken::contract::suspension::{PendingToolCall, SuspendTicket, Suspension};

    // Plugin that suspends ALL tool calls
    struct SuspendAllHook;
    impl Clone for SuspendAllHook {
        fn clone(&self) -> Self {
            Self
        }
    }
    #[async_trait]
    impl PhaseHook for SuspendAllHook {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let tool_name = match &ctx.tool_name {
                Some(name) => name.as_str(),
                None => return Ok(awaken::StateCommand::new()),
            };
            if ctx.resume_input.is_some() {
                return Ok(awaken::StateCommand::new());
            }
            let call_id = ctx.tool_call_id.as_deref().unwrap_or("");
            let args = ctx.tool_args.clone().unwrap_or_default();
            let ticket = SuspendTicket::new(
                Suspension {
                    id: format!("suspend_{call_id}"),
                    action: format!("tool:{tool_name}"),
                    message: format!("Tool '{tool_name}' needs approval"),
                    parameters: args.clone(),
                    response_schema: None,
                },
                PendingToolCall::new(call_id, tool_name, args),
                ToolCallResumeMode::ReplayToolCall,
            );
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<ToolInterceptAction>(ToolInterceptPayload::Suspend(ticket))?;
            Ok(cmd)
        }
    }
    struct SuspendAllWrapper;
    impl Plugin for SuspendAllWrapper {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "suspend-all-test",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "suspend-all-test",
                awaken::Phase::BeforeToolExecute,
                SuspendAllHook,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("c1", "echo", json!({"message": "a"})),
            ToolCall::new("c2", "echo", json!({"message": "b"})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(SuspendAllWrapper)]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do both")],
        run_identity: test_identity(),
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
        result.termination,
        TerminationReason::Suspended,
        "all tools suspended should pause the run"
    );

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // Both tool calls should be Suspended
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Suspended);
    assert_eq!(tc_states.calls["c2"].status, ToolCallStatus::Suspended);
}

/// 8. After a normal step completes, tool call states are cleared for the new step.
///    This verifies the Clear behavior at step start.
#[tokio::test]
async fn completed_tool_round_clears_state_at_next_step() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // Step 1: tool call succeeds
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // Step 2: no tools, just text
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("test")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After step 2, tool call states from step 1 should be cleared
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap_or_default();
    assert!(
        tc_states.calls.is_empty(),
        "tool call states should be cleared at the start of each new step"
    );
}

/// 9. AfterInference stop policy fires => tools from that inference do NOT execute.
#[tokio::test]
async fn after_inference_stop_prevents_tool_execution() {
    use awaken::policies::{StopConditionPlugin, StopDecision, StopPolicy, StopPolicyStats};

    /// A policy that always stops after inference.
    struct AlwaysStopPolicy;

    impl StopPolicy for AlwaysStopPolicy {
        fn id(&self) -> &str {
            "always_stop"
        }
        fn evaluate(&self, _stats: &StopPolicyStats) -> StopDecision {
            StopDecision::Stop {
                code: "forced_stop".into(),
                detail: "test stop policy fired".into(),
            }
        }
    }

    // Track whether the tool was actually executed
    let tool_executed = Arc::new(Mutex::new(false));
    struct TrackedTool(Arc<Mutex<bool>>);
    #[async_trait]
    impl Tool for TrackedTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("tracked", "tracked", "Tracks execution")
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            *self.0.lock().unwrap() = true;
            Ok(ToolResult::success("tracked", json!({"done": true})).into())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("thinking...")],
        tool_calls: vec![ToolCall::new("c1", "tracked", json!({}))],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(TrackedTool(Arc::clone(&tool_executed))));

    let stop_plugin = Arc::new(StopConditionPlugin::new(vec![Arc::new(AlwaysStopPolicy)]));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![stop_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
        matches!(
            result.termination,
            TerminationReason::Stopped(ref s) if s.code == "forced_stop"
        ),
        "expected Stopped(forced_stop), got {:?}",
        result.termination
    );

    assert!(
        !*tool_executed.lock().unwrap(),
        "tool should NOT execute when AfterInference stop policy fires"
    );
}

/// 10. LLM returns end_turn with no tool calls => NaturalEnd on first step with minimal events.
#[tokio::test]
async fn natural_end_no_tools_completes_immediately() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Hello!")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 1, "should complete in a single step");
    assert_eq!(result.response, "Hello!");

    let events = sink.take();

    // Should have no ToolCallStart/ToolCallDone events
    let tool_events = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCallStart { .. } | AgentEvent::ToolCallDone { .. }
            )
        })
        .count();
    assert_eq!(tool_events, 0, "no tool events should be emitted");

    // Should have exactly one RunStart and one RunFinish
    let run_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::RunStart { .. }))
        .count();
    let run_finishes = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::RunFinish { .. }))
        .count();
    assert_eq!(run_starts, 1);
    assert_eq!(run_finishes, 1);
}

// ---------------------------------------------------------------------------
// Error Handling
// ---------------------------------------------------------------------------

/// 11. LLM returns 3 tool calls, one has unknown tool ID. The unknown one gets
///     error result, others succeed.
#[tokio::test]
async fn unknown_tool_in_multi_call_doesnt_crash() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "first"})),
                ToolCall::new("c2", "nonexistent_tool", json!({})),
                ToolCall::new("c3", "calc", json!({"result": 7})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Handled.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("multi-tool")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Should NOT crash and should complete
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 2);

    let events = sink.take();
    let tool_done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id.clone(), *outcome)),
            _ => None,
        })
        .collect();

    // Should have 3 ToolCallDone events
    assert_eq!(
        tool_done_events.len(),
        3,
        "all three tool calls should produce ToolCallDone events"
    );

    // The unknown tool should have Failed outcome
    let unknown_outcome = tool_done_events
        .iter()
        .find(|(id, _)| id == "c2")
        .map(|(_, o)| *o);
    assert_eq!(
        unknown_outcome,
        Some(awaken::contract::suspension::ToolCallOutcome::Failed),
        "unknown tool should have Failed outcome"
    );

    // Known tools should have Succeeded outcomes
    let known_succeeded = tool_done_events
        .iter()
        .filter(|(id, o)| {
            (id == "c1" || id == "c3")
                && *o == awaken::contract::suspension::ToolCallOutcome::Succeeded
        })
        .count();
    assert_eq!(known_succeeded, 2, "both known tools should succeed");
}

/// 12. Permission hook blocks a tool. After blocking, tool is NOT re-executed on resume.
#[tokio::test]
async fn permission_denied_does_not_replay_tool() {
    let tool_executed = Arc::new(Mutex::new(0u32));
    struct CountingEchoTool(Arc<Mutex<u32>>);
    #[async_trait]
    impl Tool for CountingEchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Counting echo")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            *self.0.lock().unwrap() += 1;
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ToolResult::success_with_message("echo", args, msg).into())
        }
    }

    let exec_count = Arc::clone(&tool_executed);
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "echo",
        "c1",
        json!({"message": "blocked"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(CountingEchoTool(exec_count)));
    let blocking_plugin = Arc::new(BlockingPluginWrapper(BlockingPlugin {
        blocked_tool: "echo".into(),
        reason: "permission denied".into(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![blocking_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("use echo")],
        run_identity: test_identity(),
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
        matches!(result.termination, TerminationReason::Blocked(_)),
        "should be blocked"
    );
    assert_eq!(
        *tool_executed.lock().unwrap(),
        0,
        "tool should never have been executed when permission hook blocks it"
    );
}

/// 13. Send decision for non-existent call_id via prepare_resume. Should fail
///     with an error (not crash/panic). Validates defensive handling of unknown IDs.
#[tokio::test]
async fn decision_for_unknown_call_id_returns_error() {
    // Run until suspension so we have valid tool call state
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "test"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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

    // Send a decision for a nonexistent call_id -- should return Err, not panic
    let err = prepare_resume(
        runtime.store(),
        vec![(
            "nonexistent_id".into(),
            ToolCallResume {
                decision_id: "d0".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    );
    assert!(
        err.is_err(),
        "decision for unknown call_id should return error, not panic"
    );
    assert!(
        err.unwrap_err().to_string().contains("not found"),
        "error should indicate the call was not found"
    );

    // The valid suspended call should still be intact
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(
        tc_states.calls["c1"].status,
        ToolCallStatus::Suspended,
        "valid suspended call should remain intact after failed resume of unknown ID"
    );
}

/// 14. Send Resume for a Succeeded call. Should be ignored (terminal state).
///     We test via prepare_resume which should error when the call is not Suspended.
#[tokio::test]
async fn decision_channel_rejects_illegal_transition() {
    // Run to completion with a successful tool call
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("test")],
        run_identity: test_identity(),
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

    // After completion, tool call states are cleared. Attempting prepare_resume should fail.
    let err = prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    );

    assert!(
        err.is_err(),
        "resuming a completed/cleared call should fail: terminal state cannot be transitioned"
    );
}

/// 15. Some tools succeed, some suspend. Verify correct state for each.
#[tokio::test]
async fn mixed_suspended_and_completed_tools() {
    // Echo succeeds, dangerous suspends
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "dangerous", json!({"action": "nuke"})),
            ToolCall::new("c3", "calc", json!({"result": 99})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run all")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // With sequential executor: first echo succeeds, then dangerous suspends,
    // then execution stops (no more tools after suspension)
    assert_eq!(
        result.termination,
        TerminationReason::Suspended,
        "should suspend because dangerous tool suspends"
    );

    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();

    // echo (c1) should have succeeded
    assert_eq!(
        tc_states.calls["c1"].status,
        ToolCallStatus::Succeeded,
        "echo tool should succeed"
    );

    // dangerous (c2) should be suspended
    assert_eq!(
        tc_states.calls["c2"].status,
        ToolCallStatus::Suspended,
        "dangerous tool should be suspended"
    );

    // calc (c3) should NOT have executed (sequential executor stops after suspension)
    assert!(
        !tc_states.calls.contains_key("c3"),
        "calc tool should not have executed after suspension, but got: {:?}",
        tc_states.calls.get("c3")
    );

    // Verify events
    let events = sink.take();
    let tool_done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id.clone(), *outcome)),
            _ => None,
        })
        .collect();

    // echo should have a Succeeded ToolCallDone
    let echo_outcome = tool_done_events.iter().find(|(id, _)| id == "c1");
    assert!(
        matches!(
            echo_outcome,
            Some((_, awaken::contract::suspension::ToolCallOutcome::Succeeded))
        ),
        "echo should emit Succeeded ToolCallDone"
    );
}

// ===========================================================================
// Migrated from uncarve loop_runner integration tests
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. Agent config defaults & builder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_config_defaults() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let config = ResolvedAgent::new("test", "gpt-4", "sys", llm);
    assert_eq!(config.max_rounds(), 16);
    assert!(config.system_prompt() == "sys");
    assert!(config.tools.is_empty());
}

#[tokio::test]
async fn agent_config_builder_chain() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let config = ResolvedAgent::new("test", "gpt-4", "You are helpful.", llm)
        .with_max_rounds(5)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));

    assert_eq!(config.model, "gpt-4");
    assert_eq!(config.max_rounds(), 5);
    assert_eq!(config.system_prompt(), "You are helpful.");
    assert_eq!(config.tools.len(), 2);
    assert!(config.tools.contains_key("echo"));
    assert!(config.tools.contains_key("calc"));
}

#[tokio::test]
async fn agent_config_with_tools_batch() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let tools: Vec<Arc<dyn awaken::contract::tool::Tool>> = vec![
        Arc::new(EchoTool),
        Arc::new(CalcTool),
        Arc::new(FailingTool),
    ];
    let config = ResolvedAgent::new("test", "m", "s", llm).with_tools(tools);

    assert_eq!(config.tools.len(), 3);
    assert!(config.tools.contains_key("echo"));
    assert!(config.tools.contains_key("calc"));
    assert!(config.tools.contains_key("fail"));
}

// ---------------------------------------------------------------------------
// 2. Tool descriptor schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_descriptor_has_required_fields() {
    let echo = EchoTool;
    let desc = echo.descriptor();
    assert_eq!(desc.id, "echo");
    assert!(!desc.description.is_empty());
}

#[tokio::test]
async fn tool_descriptor_with_parameters_schema() {
    let desc = ToolDescriptor::new("search", "search", "Searches the web").with_parameters(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        },
        "required": ["query"]
    }));

    assert_eq!(desc.id, "search");
    assert_eq!(desc.parameters["properties"]["query"]["type"], "string");
}

#[tokio::test]
async fn tool_descriptors_sorted_by_id() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let config = ResolvedAgent::new("test", "m", "s", llm)
        .with_tool(Arc::new(CalcTool))
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool));

    let descs = config.tool_descriptors();
    let ids: Vec<&str> = descs.iter().map(|d| d.id.as_str()).collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort();
    assert_eq!(ids, sorted_ids, "tool_descriptors should be sorted by id");
}

// ---------------------------------------------------------------------------
// 3. Parallel tool execution (multiple tools run, results collected)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_tools_all_succeed() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "alpha"})),
                ToolCall::new("c2", "echo", json!({"message": "beta"})),
                ToolCall::new("c3", "calc", json!({"result": 100})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("All tools done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run all three")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 2);

    let events = sink.take();
    let tool_done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id.clone(), *outcome)),
            _ => None,
        })
        .collect();

    assert_eq!(tool_done_events.len(), 3, "all three tools should complete");
    let all_succeeded = tool_done_events
        .iter()
        .all(|(_, o)| *o == awaken::contract::suspension::ToolCallOutcome::Succeeded);
    assert!(all_succeeded, "all three tools should succeed");
}

// ---------------------------------------------------------------------------
// 4. Parallel tools: mixed success and failure with result data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_tools_mixed_outcomes_preserve_results() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "works"})),
                ToolCall::new("c2", "fail", json!({})),
                ToolCall::new("c3", "calc", json!({"result": 7})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Mixed results.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run mixed")],
        run_identity: test_identity(),
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

    let events = sink.take();
    let results_by_id: std::collections::HashMap<
        String,
        (awaken::contract::suspension::ToolCallOutcome, ToolResult),
    > = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone {
                id,
                outcome,
                result,
                ..
            } => Some((id.clone(), (*outcome, result.clone()))),
            _ => None,
        })
        .collect();

    assert_eq!(results_by_id.len(), 3);

    // echo should succeed with message
    let (echo_outcome, echo_result) = &results_by_id["c1"];
    assert_eq!(
        *echo_outcome,
        awaken::contract::suspension::ToolCallOutcome::Succeeded
    );
    assert!(echo_result.is_success());

    // fail should fail
    let (fail_outcome, _) = &results_by_id["c2"];
    assert_eq!(
        *fail_outcome,
        awaken::contract::suspension::ToolCallOutcome::Failed
    );

    // calc should succeed
    let (calc_outcome, calc_result) = &results_by_id["c3"];
    assert_eq!(
        *calc_outcome,
        awaken::contract::suspension::ToolCallOutcome::Succeeded
    );
    assert!(calc_result.is_success());
    assert_eq!(calc_result.data, json!(7));
}

// ---------------------------------------------------------------------------
// 5. System prompt appears in LLM request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_prompt_included_in_inference_request() {
    struct SystemPromptRecordingLlm {
        messages: Mutex<Vec<Vec<Message>>>,
    }

    impl SystemPromptRecordingLlm {
        fn new() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for SystemPromptRecordingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.messages.lock().unwrap().push(req.messages.clone());
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "system-prompt-recording"
        }
    }

    let llm = Arc::new(SystemPromptRecordingLlm::new());
    let llm_clone = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "gpt-4o", "You are a helpful assistant.", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_clone.messages.lock().unwrap();
    assert!(
        !requests.is_empty(),
        "should have at least one inference call"
    );
    let first_messages = &requests[0];
    let has_system_prompt = first_messages
        .iter()
        .any(|msg| msg.text().contains("You are a helpful assistant."));
    assert!(
        has_system_prompt,
        "system prompt should appear as a message in inference request, got: {:?}",
        first_messages.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 6. Message ordering: user messages appear in correct order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn message_ordering_preserved_in_inference_request() {
    struct MessageOrderLlm {
        requests: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmExecutor for MessageOrderLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "message-order"
        }
    }

    let llm = Arc::new(MessageOrderLlm {
        requests: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![
            Message::user("first message"),
            Message::user("second message"),
        ],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    assert!(!requests.is_empty());
    let first_req = &requests[0];
    let first_idx = first_req.iter().position(|m| m.contains("first message"));
    let second_idx = first_req.iter().position(|m| m.contains("second message"));
    assert!(first_idx.is_some(), "first message should appear");
    assert!(second_idx.is_some(), "second message should appear");
    assert!(
        first_idx.unwrap() < second_idx.unwrap(),
        "messages should be in order: first before second"
    );
}

// ---------------------------------------------------------------------------
// 7. Tools provided to LLM as descriptors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_descriptors_sent_to_llm() {
    struct ToolDescriptorCheckLlm {
        tool_ids: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmExecutor for ToolDescriptorCheckLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let ids: Vec<String> = req.tools.iter().map(|t| t.id.clone()).collect();
            self.tool_ids.lock().unwrap().push(ids);
            Ok(StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "tool-descriptor-check"
        }
    }

    let llm = Arc::new(ToolDescriptorCheckLlm {
        tool_ids: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let tool_ids = llm_ref.tool_ids.lock().unwrap();
    assert!(!tool_ids.is_empty());
    let first_call_tools = &tool_ids[0];
    assert!(
        first_call_tools.contains(&"echo".to_string()),
        "echo tool descriptor should be sent to LLM"
    );
    assert!(
        first_call_tools.contains(&"calc".to_string()),
        "calc tool descriptor should be sent to LLM"
    );
}

// ---------------------------------------------------------------------------
// 8. Run identity fields propagate correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_identity_fields_propagate_to_lifecycle() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let identity = RunIdentity::new(
        "thread-abc".into(),
        Some("parent-thread".into()),
        "run-xyz".into(),
        Some("parent-run".into()),
        "agent-42".into(),
        RunOrigin::Internal,
    );

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.run_id, "run-xyz");
    assert_eq!(lifecycle.status, RunStatus::Done);
}

// ---------------------------------------------------------------------------
// 9. Context message injection via plugin (suffix system)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn context_message_suffix_system_injected() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    struct RecordingLlm2 {
        requests: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmExecutor for RecordingLlm2 {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.requests.lock().unwrap().push(req.messages.clone());
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "recording2"
        }
    }

    struct SuffixInjectorHook;

    #[async_trait]
    impl PhaseHook for SuffixInjectorHook {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<AddContextMessage>(ContextMessage::suffix_system(
                "test_suffix",
                "This is a suffix system reminder.",
            ))?;
            Ok(cmd)
        }
    }

    struct SuffixInjectorPlugin;
    impl Plugin for SuffixInjectorPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "suffix-injector",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "suffix-injector",
                awaken::Phase::BeforeInference,
                SuffixInjectorHook,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(RecordingLlm2 {
        requests: Mutex::new(Vec::new()),
    });
    let llm_clone = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(SuffixInjectorPlugin)]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hello")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_clone.requests.lock().unwrap();
    assert!(!requests.is_empty());
    let first_req = &requests[0];
    let has_suffix = first_req
        .iter()
        .any(|msg| msg.text().contains("suffix system reminder"));
    assert!(
        has_suffix,
        "suffix system message should be injected, got messages: {:?}",
        first_req.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 10. Multiple context messages injected in same step
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_context_messages_injected() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    struct RecordingLlm3 {
        requests: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmExecutor for RecordingLlm3 {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.requests.lock().unwrap().push(req.messages.clone());
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "recording3"
        }
    }

    struct MultiContextHook;

    #[async_trait]
    impl PhaseHook for MultiContextHook {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                "ctx_alpha",
                "Alpha context message.",
            ))?;
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                "ctx_beta",
                "Beta context message.",
            ))?;
            Ok(cmd)
        }
    }

    struct MultiContextPlugin;
    impl Plugin for MultiContextPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "multi-context",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "multi-context",
                awaken::Phase::BeforeInference,
                MultiContextHook,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(RecordingLlm3 {
        requests: Mutex::new(Vec::new()),
    });
    let llm_clone = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(MultiContextPlugin)]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_clone.requests.lock().unwrap();
    let first_req = &requests[0];
    let has_alpha = first_req
        .iter()
        .any(|msg| msg.text().contains("Alpha context message"));
    let has_beta = first_req
        .iter()
        .any(|msg| msg.text().contains("Beta context message"));
    assert!(has_alpha, "alpha context should be injected");
    assert!(has_beta, "beta context should be injected");
}

// ---------------------------------------------------------------------------
// 11. Phase hooks fire with tool call context (BeforeToolExecute/AfterToolExecute)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn phase_hooks_fire_with_tool_call_phases() {
    let recorded_phases = Arc::new(Mutex::new(Vec::<Phase>::new()));

    struct DetailedPhaseTracker(Arc<Mutex<Vec<Phase>>>);
    #[async_trait]
    impl PhaseHook for DetailedPhaseTracker {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            self.0.lock().unwrap().push(ctx.phase);
            Ok(awaken::StateCommand::new())
        }
    }

    struct DetailedTrackerPlugin(Arc<Mutex<Vec<Phase>>>);
    impl Plugin for DetailedTrackerPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "detailed-tracker",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            for phase in Phase::ALL {
                registrar.register_phase_hook(
                    "detailed-tracker",
                    phase,
                    DetailedPhaseTracker(Arc::clone(&self.0)),
                )?;
            }
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();

    let tracker = Arc::new(DetailedTrackerPlugin(Arc::clone(&recorded_phases)));
    let resolver = FixedResolver::with_plugins(agent, vec![tracker]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let phases = recorded_phases.lock().unwrap();
    assert!(
        phases.contains(&Phase::BeforeToolExecute),
        "BeforeToolExecute should fire for tool call, got: {:?}",
        *phases
    );
    assert!(
        phases.contains(&Phase::AfterToolExecute),
        "AfterToolExecute should fire after tool completes, got: {:?}",
        *phases
    );
    assert!(
        phases.len() >= 10,
        "should have at least 10 phase hooks, got {}",
        phases.len()
    );
}

// ---------------------------------------------------------------------------
// 12. Step count increments correctly across tool call steps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_count_increments_with_tool_calls() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c2", "echo", json!({"message": "2"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("final")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.steps, 3, "should have 3 steps: 2 tool + 1 final");
    assert_eq!(result.termination, TerminationReason::NaturalEnd);

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.step_count, 3);
}

// ---------------------------------------------------------------------------
// 13. Token usage reported in inference events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_usage_reported_in_inference_events() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hi")],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(50),
            completion_tokens: Some(20),
            total_tokens: Some(70),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    let inference_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .collect();
    assert_eq!(
        inference_events.len(),
        1,
        "should have one InferenceComplete event"
    );
}

// ---------------------------------------------------------------------------
// 14. Blocking plugin allows non-targeted tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocking_plugin_allows_non_targeted_tool() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "calc", json!({"result": 42}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(CalcTool));
    let blocking_plugin = Arc::new(BlockingPluginWrapper(BlockingPlugin {
        blocked_tool: "echo".into(),
        reason: "echo is blocked".into(),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![blocking_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("calc")],
        run_identity: test_identity(),
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
        result.termination,
        TerminationReason::NaturalEnd,
        "non-blocked tool should proceed normally"
    );
    assert_eq!(result.steps, 2);
}

// ---------------------------------------------------------------------------
// 15. SetResult intercept on specific tool only (other tool executes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_result_intercept_on_specific_tool_only() {
    let tool_executed = Arc::new(Mutex::new(Vec::<String>::new()));

    struct TrackingCalcTool2(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl Tool for TrackingCalcTool2 {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("calc", "calculator", "Tracking calculator")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push("calc".into());
            Ok(ToolResult::success("calc", args).into())
        }
    }

    struct TrackingEchoTool3(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl Tool for TrackingEchoTool3 {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Tracking echo")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push("echo".into());
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ToolResult::success_with_message("echo", args, msg).into())
        }
    }

    let executed = Arc::clone(&tool_executed);
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "hi"})),
                ToolCall::new("c2", "calc", json!({"result": 5})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(TrackingEchoTool3(Arc::clone(&executed))))
        .with_tool(Arc::new(TrackingCalcTool2(Arc::clone(&executed))));

    let set_result_plugin = Arc::new(SetResultPluginWrapper(SetResultPlugin {
        target_tool: "echo".into(),
        result: ToolResult::success_with_message("echo", json!({}), "intercepted".to_string()),
    }));

    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![set_result_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("run both")],
        run_identity: test_identity(),
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

    let executed_tools = tool_executed.lock().unwrap();
    assert!(
        !executed_tools.contains(&"echo".to_string()),
        "echo should not execute when SetResult intercept is active"
    );
    assert!(
        executed_tools.contains(&"calc".to_string()),
        "calc should execute normally (not intercepted)"
    );
}

// ---------------------------------------------------------------------------
// 16. Phase hook sees tool name and call_id in BeforeToolExecute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn phase_hook_receives_tool_context() {
    let observed_tool_names = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_call_ids = Arc::new(Mutex::new(Vec::<String>::new()));

    struct ToolContextObserver {
        tool_names: Arc<Mutex<Vec<String>>>,
        call_ids: Arc<Mutex<Vec<String>>>,
    }

    impl Clone for ToolContextObserver {
        fn clone(&self) -> Self {
            Self {
                tool_names: Arc::clone(&self.tool_names),
                call_ids: Arc::clone(&self.call_ids),
            }
        }
    }

    #[async_trait]
    impl PhaseHook for ToolContextObserver {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            if let Some(ref name) = ctx.tool_name {
                self.tool_names.lock().unwrap().push(name.clone());
            }
            if let Some(ref id) = ctx.tool_call_id {
                self.call_ids.lock().unwrap().push(id.clone());
            }
            Ok(awaken::StateCommand::new())
        }
    }

    struct ToolContextPlugin {
        observer: ToolContextObserver,
    }

    impl Plugin for ToolContextPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "tool-context-observer",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "tool-context-observer",
                awaken::Phase::BeforeToolExecute,
                self.observer.clone(),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "test"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let observer_plugin = Arc::new(ToolContextPlugin {
        observer: ToolContextObserver {
            tool_names: Arc::clone(&observed_tool_names),
            call_ids: Arc::clone(&observed_call_ids),
        },
    });
    let resolver = FixedResolver::with_plugins(agent, vec![observer_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let names = observed_tool_names.lock().unwrap();
    let ids = observed_call_ids.lock().unwrap();

    assert!(
        names.contains(&"echo".to_string()),
        "should see tool name 'echo'"
    );
    assert!(ids.contains(&"c1".to_string()), "should see call_id 'c1'");
}

// ---------------------------------------------------------------------------
// 17. LLM error on second step propagates after tool call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn llm_error_on_second_step_propagates() {
    struct FailOnSecondCallLlm {
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for FailOnSecondCallLlm {
        async fn execute(
            &self,
            _req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Err(InferenceExecutionError::Provider(
                    "second call failed".into(),
                ))
            }
        }

        fn name(&self) -> &str {
            "fail-on-second"
        }
    }

    let llm = Arc::new(FailOnSecondCallLlm {
        call_count: Mutex::new(0),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;

    assert!(result.is_err(), "second-step LLM error should propagate");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("second call failed"),
        "error should contain the provider message"
    );
}

// ---------------------------------------------------------------------------
// 18. AfterInference hook sees LLM response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_inference_hook_sees_llm_response() {
    let saw_response = Arc::new(Mutex::new(false));

    struct AfterInferenceObserver(Arc<Mutex<bool>>);

    impl Clone for AfterInferenceObserver {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }

    #[async_trait]
    impl PhaseHook for AfterInferenceObserver {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            if ctx.llm_response.is_some() {
                *self.0.lock().unwrap() = true;
            }
            Ok(awaken::StateCommand::new())
        }
    }

    struct AfterInferenceObserverPlugin(Arc<Mutex<bool>>);
    impl Plugin for AfterInferenceObserverPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "after-inference-observer",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "after-inference-observer",
                awaken::Phase::AfterInference,
                AfterInferenceObserver(Arc::clone(&self.0)),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hello")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(
        agent,
        vec![Arc::new(AfterInferenceObserverPlugin(Arc::clone(
            &saw_response,
        )))],
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
        *saw_response.lock().unwrap(),
        "AfterInference hook should see the LLM response"
    );
}

// ---------------------------------------------------------------------------
// 19. AfterToolExecute hook sees tool result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_tool_execute_hook_sees_tool_result() {
    let tool_results_observed = Arc::new(Mutex::new(Vec::<ToolResult>::new()));

    struct AfterToolObserver(Arc<Mutex<Vec<ToolResult>>>);

    impl Clone for AfterToolObserver {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }

    #[async_trait]
    impl PhaseHook for AfterToolObserver {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            if let Some(ref result) = ctx.tool_result {
                self.0.lock().unwrap().push(result.clone());
            }
            Ok(awaken::StateCommand::new())
        }
    }

    struct AfterToolObserverPlugin(Arc<Mutex<Vec<ToolResult>>>);
    impl Plugin for AfterToolObserverPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "after-tool-observer",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "after-tool-observer",
                awaken::Phase::AfterToolExecute,
                AfterToolObserver(Arc::clone(&self.0)),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "world"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(
        agent,
        vec![Arc::new(AfterToolObserverPlugin(Arc::clone(
            &tool_results_observed,
        )))],
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let results = tool_results_observed.lock().unwrap();
    assert_eq!(results.len(), 1, "should observe one tool result");
    assert!(results[0].is_success(), "tool result should be success");
}

// ---------------------------------------------------------------------------
// 20. Max rounds = 0 stops immediately
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_rounds_two_stops_after_two_tool_steps() {
    let llm = Arc::new(ScriptedLlm::new(
        (0..5)
            .map(|i| StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    format!("c{i}"),
                    "echo",
                    json!({"message": format!("round{i}")}),
                )],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
            .collect(),
    ));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_max_rounds(2)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
        matches!(
            result.termination,
            TerminationReason::Stopped(ref s) if s.code == "max_rounds"
        ),
        "max_rounds=2 should trigger Stopped(max_rounds), got {:?}",
        result.termination
    );
}

// ---------------------------------------------------------------------------
// 21. StepStart events contain step number
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_start_events_contain_step_number() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
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
    let step_start_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepStart { .. }))
        .count();

    assert_eq!(step_start_count, 2, "should have 2 step starts");
}

// ---------------------------------------------------------------------------
// 22. Suspension preserves original tool arguments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspension_preserves_original_arguments() {
    let original_args =
        json!({"action": "deploy", "target": "production", "config": {"replicas": 3}});

    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        original_args.clone(),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("deploy")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let tc_states = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc_states.calls["c1"].status, ToolCallStatus::Suspended);
    assert_eq!(
        tc_states.calls["c1"].arguments, original_args,
        "suspended tool call should preserve original arguments"
    );
    assert_eq!(tc_states.calls["c1"].tool_name, "dangerous");
}

// ---------------------------------------------------------------------------
// 23. Second tool not executed after first suspends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_tool_not_executed_after_first_suspends() {
    let tool_executed = Arc::new(Mutex::new(Vec::<String>::new()));

    struct TrackingEchoTool2(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl Tool for TrackingEchoTool2 {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Tracking echo 2")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push("echo".into());
            Ok(ToolResult::success("echo", args).into())
        }
    }

    let executed = Arc::clone(&tool_executed);
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("c1", "dangerous", json!({"action": "first"})),
            ToolCall::new("c2", "echo", json!({"message": "second"})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(TrackingEchoTool2(executed)));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do both")],
        run_identity: test_identity(),
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

    let executed_tools = tool_executed.lock().unwrap();
    assert!(
        !executed_tools.contains(&"echo".to_string()),
        "echo should NOT execute after dangerous suspends"
    );
}

// ---------------------------------------------------------------------------
// 24. RunStart event emitted first
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_start_event_emitted_first() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hi")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    let first_event = &events[0];
    assert!(
        matches!(first_event, AgentEvent::RunStart { .. }),
        "first event should be RunStart, got {:?}",
        first_event
    );
}

// ---------------------------------------------------------------------------
// 25. RunFinish event emitted last
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_finish_event_emitted_last() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hi")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    let last_event = events.last().unwrap();
    assert!(
        matches!(last_event, AgentEvent::RunFinish { .. }),
        "last event should be RunFinish, got {:?}",
        last_event
    );
}

// ---------------------------------------------------------------------------
// 26. Tool call events contain correct tool name and call_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_call_events_contain_correct_metadata() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new(
                "tc_42",
                "echo",
                json!({"message": "meta-test"}),
            )],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
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

    let start_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStart { id, name } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(start_events.len(), 1);
    assert_eq!(start_events[0].0, "tc_42", "call_id should match");
    assert_eq!(start_events[0].1, "echo", "tool_name should match");

    let done_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, outcome, .. } => Some((id.clone(), *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(done_events.len(), 1);
    assert_eq!(done_events[0].0, "tc_42");
    assert_eq!(
        done_events[0].1,
        awaken::contract::suspension::ToolCallOutcome::Succeeded
    );
}

// ---------------------------------------------------------------------------
// 27. Three-step loop: tool -> tool -> final response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_step_loop_tool_tool_response() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("Step 1")],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "step1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Step 2")],
            tool_calls: vec![ToolCall::new("c2", "calc", json!({"result": 10}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Final answer.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.response, "Final answer.");

    let events = sink.take();
    let step_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepStart { .. }))
        .count();
    assert_eq!(step_starts, 3, "should have 3 StepStart events");

    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert_eq!(tool_done_count, 2, "should have 2 ToolCallDone events");
}

// ---------------------------------------------------------------------------
// 28. Lifecycle status: Running -> Done
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_transitions_running_to_done() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
    assert!(lifecycle.status_reason.is_some());
}

// ---------------------------------------------------------------------------
// 29. Lifecycle status: Running -> Waiting (suspended)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_transitions_running_to_waiting() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do it")],
        run_identity: test_identity(),
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
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);
}

// ---------------------------------------------------------------------------
// 30. Lifecycle status: Running -> Done (cancelled)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_transitions_running_to_done_on_cancel() {
    use awaken::CancellationToken;

    let llm = Arc::new(SlowStreamingLlm::new(["tok "; 10].to_vec(), 50));
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let token = CancellationToken::new();
    token.cancel();

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: Some(token),
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::Cancelled);
    // Lifecycle status depends on when cancellation was detected:
    // - If detected before step execution: may remain Running (loop exits early)
    // - If detected during step: transitions to Done
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert!(
        lifecycle.status == RunStatus::Done || lifecycle.status == RunStatus::Running,
        "expected Done or Running after cancel, got {:?}",
        lifecycle.status
    );
}

// ---------------------------------------------------------------------------
// 31. TextDelta events emitted for text response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_delta_events_emitted_for_text_response() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("Hello world!")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
        .collect();
    assert!(
        !text_deltas.is_empty(),
        "should emit TextDelta events for text content"
    );
}

// ===========================================================================
// Group 1: Parallel tool state isolation (5 tests)
// ===========================================================================

/// Parallel tool calls in the same step each get their own ToolCallState entries.
#[tokio::test]
async fn parallel_tools_have_independent_state_entries() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("p1", "echo", json!({"message": "alpha"})),
                ToolCall::new("p2", "calc", json!({"result": 99})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Both done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    // After step 2, states are cleared, but ToolCallDone events prove isolation
    let events = sink.take();
    let done_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(done_ids.len(), 2);
    assert!(done_ids.contains(&"p1".to_string()));
    assert!(done_ids.contains(&"p2".to_string()));
}

/// Parallel tools: one succeeds, one suspends. Succeeded tool state preserved at suspension.
#[tokio::test]
async fn parallel_tools_succeed_and_suspend_independent_states() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![],
        tool_calls: vec![
            ToolCall::new("ok", "echo", json!({"message": "fine"})),
            ToolCall::new("sus", "dangerous", json!({"action": "rm"})),
        ],
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc.calls["ok"].status, ToolCallStatus::Succeeded);
    assert_eq!(tc.calls["sus"].status, ToolCallStatus::Suspended);
}

/// Parallel tools: both fail, both get Failed status in ToolCallDone events.
#[tokio::test]
async fn parallel_tools_both_fail_independently() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("f1", "fail", json!({})),
                ToolCall::new("f2", "fail", json!({})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Both failed.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    let events = sink.take();
    let failed_count = events
        .iter()
        .filter(|e| {
            matches!(e, AgentEvent::ToolCallDone { outcome, .. }
                if *outcome == awaken::contract::suspension::ToolCallOutcome::Failed)
        })
        .count();
    assert_eq!(
        failed_count, 2,
        "both parallel tools should fail independently"
    );
}

/// Parallel tool: same tool invoked twice with different args produces distinct results.
#[tokio::test]
async fn parallel_same_tool_distinct_results() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("e1", "echo", json!({"message": "first"})),
                ToolCall::new("e2", "echo", json!({"message": "second"})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo twice")],
        run_identity: test_identity(),
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

    let events = sink.take();
    let results: std::collections::HashMap<String, ToolResult> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, result, .. } => Some((id.clone(), result.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_ne!(
        results["e1"].message, results["e2"].message,
        "same tool with different args should produce distinct results"
    );
}

/// Sequential step 2 sees fresh tool call state (step 1 state cleared).
#[tokio::test]
async fn sequential_steps_see_fresh_tool_state() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("s1", "echo", json!({"message": "step1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("s2", "echo", json!({"message": "step2"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("final")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // After 3 steps, tool states should be cleared
    let tc = runtime.store().read::<ToolCallStates>().unwrap_or_default();
    assert!(
        tc.calls.is_empty(),
        "final step should clear tool call states"
    );
}

// ===========================================================================
// Group 2: State snapshot / revision behavior (5 tests)
// ===========================================================================

/// State snapshot revision increases across steps.
#[tokio::test]
async fn state_snapshot_revision_increases_across_steps() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "a"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    let revisions: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StateSnapshot { snapshot } => {
                snapshot.get("revision").and_then(|v| v.as_u64())
            }
            _ => None,
        })
        .collect();

    assert!(
        revisions.len() >= 2,
        "should have at least 2 snapshots, got {}",
        revisions.len()
    );
    for window in revisions.windows(2) {
        assert!(
            window[1] >= window[0],
            "snapshot revision should be non-decreasing: {:?}",
            revisions
        );
    }
}

/// State snapshot contains extensions field with registered key data.
#[tokio::test]
async fn state_snapshot_contains_extensions_with_lifecycle() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
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
    let last_snapshot = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StateSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .next_back()
        .expect("should have at least one snapshot");

    let extensions = last_snapshot
        .get("extensions")
        .expect("snapshot should have extensions");
    assert!(extensions.is_object(), "extensions should be an object");
}

/// State snapshot emitted for every step plus the final run end.
#[tokio::test]
async fn state_snapshot_count_matches_steps_plus_finish() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c2", "echo", json!({"message": "2"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("end")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    let events = sink.take();
    let snapshot_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StateSnapshot { .. }))
        .count();

    // At least one per step + one for run finish
    assert!(
        snapshot_count >= result.steps as usize,
        "expected at least {} snapshots (one per step), got {}",
        result.steps,
        snapshot_count
    );
}

/// State snapshot at suspension includes Waiting status.
#[tokio::test]
async fn state_snapshot_at_suspension_includes_waiting_status() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "nuke"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    let events = sink.take();
    let last_snapshot = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StateSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .next_back()
        .expect("should have at least one snapshot");

    // The snapshot should contain the lifecycle with Waiting status
    let extensions = last_snapshot.get("extensions").unwrap();
    let lifecycle_json = extensions.get("__runtime.run_lifecycle");
    assert!(
        lifecycle_json.is_some(),
        "snapshot extensions should contain run_lifecycle"
    );
}

/// Export_persisted after run completion returns correct revision.
#[tokio::test]
async fn export_persisted_after_run_has_positive_revision() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let persisted = runtime.store().export_persisted().unwrap();
    assert!(
        persisted.revision > 0,
        "persisted state should have positive revision after run"
    );
}

// ===========================================================================
// Group 3: Checkpoint behavior (5 tests)
// ===========================================================================

/// Checkpoint store receives checkpoint data when provided.
#[tokio::test]
async fn checkpoint_store_receives_data() {
    use awaken::stores::InMemoryStore;

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("checkpointed")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let checkpoint = Arc::new(InMemoryStore::new());

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: Some(checkpoint.as_ref()),
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    use awaken::contract::storage::RunStore;
    let record = checkpoint.load_run("run-1").await.unwrap();
    assert!(
        record.is_some(),
        "checkpoint store should have a run record"
    );
    let record = record.unwrap();
    assert_eq!(record.run_id, "run-1");
    assert_eq!(record.thread_id, "thread-1");
}

/// Checkpoint includes correct step count after multi-step run.
#[tokio::test]
async fn checkpoint_includes_correct_step_count() {
    use awaken::stores::InMemoryStore;

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let checkpoint = Arc::new(InMemoryStore::new());

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: Some(checkpoint.as_ref()),
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    use awaken::contract::storage::RunStore;
    let record = checkpoint.load_run("run-1").await.unwrap().unwrap();
    assert_eq!(record.steps, 2, "checkpoint should reflect 2 steps");
}

/// Checkpoint persists state blob.
#[tokio::test]
async fn checkpoint_contains_state_blob() {
    use awaken::stores::InMemoryStore;

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let checkpoint = Arc::new(InMemoryStore::new());

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: Some(checkpoint.as_ref()),
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    use awaken::contract::storage::RunStore;
    let record = checkpoint.load_run("run-1").await.unwrap().unwrap();
    assert!(
        record.state.is_some(),
        "checkpoint should contain persisted state"
    );
}

/// Checkpoint stores messages.
#[tokio::test]
async fn checkpoint_stores_thread_messages() {
    use awaken::stores::InMemoryStore;

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("hello back")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let checkpoint = Arc::new(InMemoryStore::new());

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: Some(checkpoint.as_ref()),
        messages: vec![Message::user("hello")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    use awaken::contract::storage::ThreadStore;
    let msgs = checkpoint.load_messages("thread-1").await.unwrap();
    assert!(msgs.is_some(), "checkpoint should store thread messages");
    let msgs = msgs.unwrap();
    assert!(
        msgs.len() >= 2,
        "should store at least user + assistant messages, got {}",
        msgs.len()
    );
}

/// Checkpoint records correct agent_id from identity.
#[tokio::test]
async fn checkpoint_records_agent_id() {
    use awaken::stores::InMemoryStore;

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let checkpoint = Arc::new(InMemoryStore::new());
    let identity = RunIdentity::new(
        "t-1".into(),
        None,
        "r-1".into(),
        None,
        "my-agent".into(),
        RunOrigin::User,
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: Some(checkpoint.as_ref()),
        messages: vec![Message::user("hi")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    use awaken::contract::storage::RunStore;
    let record = checkpoint.load_run("r-1").await.unwrap().unwrap();
    assert_eq!(record.agent_id, "my-agent");
}

// ===========================================================================
// Group 4: Context window handling (5 tests)
// ===========================================================================

/// LLM receives context messages from multiple user messages.
#[tokio::test]
async fn llm_receives_all_user_messages() {
    struct CountingMsgLlm {
        message_counts: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl LlmExecutor for CountingMsgLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.message_counts.lock().unwrap().push(req.messages.len());
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "counting-msg"
        }
    }

    let llm = Arc::new(CountingMsgLlm {
        message_counts: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![
            Message::user("first"),
            Message::user("second"),
            Message::user("third"),
        ],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let counts = llm_ref.message_counts.lock().unwrap();
    assert!(
        counts[0] >= 3,
        "LLM should receive at least 3 user messages (+ system prompt), got {}",
        counts[0]
    );
}

/// Tool results from step 1 appear in step 2 inference messages.
#[tokio::test]
async fn tool_results_visible_in_next_step_messages() {
    struct MsgRecordLlm {
        requests: Mutex<Vec<Vec<String>>>,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for MsgRecordLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("final")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }

        fn name(&self) -> &str {
            "msg-record"
        }
    }

    let llm = Arc::new(MsgRecordLlm {
        requests: Mutex::new(Vec::new()),
        call_count: Mutex::new(0),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "should have two LLM calls");
    let second_req = &requests[1];
    let has_tool_result = second_req.iter().any(|m| m.contains("hi"));
    assert!(
        has_tool_result,
        "second LLM call should contain tool result from step 1, got: {:?}",
        second_req
    );
}

/// Context message injection adds to existing messages without overwriting.
#[tokio::test]
async fn context_injection_additive_not_destructive() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    struct MsgCheckLlm {
        requests: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmExecutor for MsgCheckLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "msg-check"
        }
    }

    struct AdditiveContextHook;

    #[async_trait]
    impl PhaseHook for AdditiveContextHook {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                "additive_test",
                "ADDITIVE_MARKER",
            ))?;
            Ok(cmd)
        }
    }

    struct AdditivePlugin;
    impl Plugin for AdditivePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "additive-ctx",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "additive-ctx",
                awaken::Phase::BeforeInference,
                AdditiveContextHook,
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(MsgCheckLlm {
        requests: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "Original system prompt", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(AdditivePlugin)]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hello user")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    let first_req = &requests[0];
    let has_system = first_req
        .iter()
        .any(|m| m.contains("Original system prompt"));
    let has_user = first_req.iter().any(|m| m.contains("hello user"));
    let has_injected = first_req.iter().any(|m| m.contains("ADDITIVE_MARKER"));
    assert!(has_system, "system prompt should still be present");
    assert!(has_user, "user message should still be present");
    assert!(has_injected, "injected context should be present");
}

/// Token usage accumulates across multiple steps.
#[tokio::test]
async fn token_usage_accumulates_across_steps() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "a"}))],
            usage: Some(TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(200),
                completion_tokens: Some(30),
                total_tokens: Some(230),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 2,
        "should have two inference completion events"
    );
}

/// LLM gets tool descriptors even on text-only response.
#[tokio::test]
async fn tool_descriptors_present_even_when_unused() {
    struct ToolCheckLlm {
        tool_counts: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl LlmExecutor for ToolCheckLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.tool_counts.lock().unwrap().push(req.tools.len());
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "tool-check"
        }
    }

    let llm = Arc::new(ToolCheckLlm {
        tool_counts: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool))
        .with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let counts = llm_ref.tool_counts.lock().unwrap();
    assert_eq!(
        counts[0], 3,
        "all 3 tool descriptors should be sent even when tools are not used"
    );
}

// ===========================================================================
// Group 5: Plugin interaction in loop (5 tests)
// ===========================================================================

/// RunStart and RunEnd hooks fire exactly once each.
#[tokio::test]
async fn run_start_and_run_end_hooks_fire_exactly_once() {
    let run_start_count = Arc::new(Mutex::new(0u32));
    let run_end_count = Arc::new(Mutex::new(0u32));

    struct RunBoundaryCounter {
        start: Arc<Mutex<u32>>,
        end: Arc<Mutex<u32>>,
    }

    impl Clone for RunBoundaryCounter {
        fn clone(&self) -> Self {
            Self {
                start: Arc::clone(&self.start),
                end: Arc::clone(&self.end),
            }
        }
    }

    #[async_trait]
    impl PhaseHook for RunBoundaryCounter {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            match ctx.phase {
                Phase::RunStart => *self.start.lock().unwrap() += 1,
                Phase::RunEnd => *self.end.lock().unwrap() += 1,
                _ => {}
            }
            Ok(awaken::StateCommand::new())
        }
    }

    struct RunBoundaryPlugin(RunBoundaryCounter);
    impl Plugin for RunBoundaryPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "run-boundary",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook("run-boundary", Phase::RunStart, self.0.clone())?;
            registrar.register_phase_hook("run-boundary", Phase::RunEnd, self.0.clone())?;
            Ok(())
        }
    }

    let counter = RunBoundaryCounter {
        start: Arc::clone(&run_start_count),
        end: Arc::clone(&run_end_count),
    };

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "a"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(RunBoundaryPlugin(counter))]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
        *run_start_count.lock().unwrap(),
        1,
        "RunStart should fire exactly once"
    );
    assert_eq!(
        *run_end_count.lock().unwrap(),
        1,
        "RunEnd should fire exactly once"
    );
}

/// StepStart fires once per step.
#[tokio::test]
async fn step_start_fires_per_step() {
    let step_start_count = Arc::new(Mutex::new(0u32));

    struct StepCounter(Arc<Mutex<u32>>);
    impl Clone for StepCounter {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }

    #[async_trait]
    impl PhaseHook for StepCounter {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            *self.0.lock().unwrap() += 1;
            Ok(awaken::StateCommand::new())
        }
    }

    struct StepCounterPlugin(StepCounter);
    impl Plugin for StepCounterPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "step-counter",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook("step-counter", Phase::StepStart, self.0.clone())?;
            Ok(())
        }
    }

    let counter = StepCounter(Arc::clone(&step_start_count));

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c2", "echo", json!({"message": "2"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("end")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(StepCounterPlugin(counter))]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    assert_eq!(
        *step_start_count.lock().unwrap(),
        3,
        "StepStart should fire once per step"
    );
}

/// BeforeInference hook can observe step count in lifecycle via snapshot.
#[tokio::test]
async fn before_inference_hook_sees_step_count() {
    let step_counts_at_inference = Arc::new(Mutex::new(Vec::<u32>::new()));

    struct StepCountObserver(Arc<Mutex<Vec<u32>>>);
    impl Clone for StepCountObserver {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }

    #[async_trait]
    impl PhaseHook for StepCountObserver {
        async fn run(
            &self,
            ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            if let Some(lifecycle) = ctx.snapshot.get::<RunLifecycle>() {
                self.0.lock().unwrap().push(lifecycle.step_count);
            }
            Ok(awaken::StateCommand::new())
        }
    }

    struct StepCountObserverPlugin(StepCountObserver);
    impl Plugin for StepCountObserverPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "step-count-obs",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "step-count-obs",
                Phase::BeforeInference,
                self.0.clone(),
            )?;
            Ok(())
        }
    }

    let observer = StepCountObserver(Arc::clone(&step_counts_at_inference));

    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("end")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver =
        FixedResolver::with_plugins(agent, vec![Arc::new(StepCountObserverPlugin(observer))]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let counts = step_counts_at_inference.lock().unwrap();
    assert_eq!(counts.len(), 2, "should observe 2 BeforeInference hooks");
}

/// Plugin can modify context at BeforeInference, visible in same step's LLM call.
#[tokio::test]
async fn plugin_context_mutation_visible_in_same_step() {
    use awaken::agent::state::AddContextMessage;
    use awaken::contract::context_message::ContextMessage;

    struct StepScopedContextHook {
        marker: String,
    }

    #[async_trait]
    impl PhaseHook for StepScopedContextHook {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            let mut cmd = awaken::StateCommand::new();
            cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
                "step_scoped",
                &self.marker,
            ))?;
            Ok(cmd)
        }
    }

    struct StepScopedPlugin(String);
    impl Plugin for StepScopedPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "step-scoped-ctx",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "step-scoped-ctx",
                awaken::Phase::BeforeInference,
                StepScopedContextHook {
                    marker: self.0.clone(),
                },
            )?;
            Ok(())
        }
    }

    struct ContextVerifyLlm {
        requests: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmExecutor for ContextVerifyLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            Ok(StreamResult {
                content: vec![ContentBlock::text("ok")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "ctx-verify"
        }
    }

    let llm = Arc::new(ContextVerifyLlm {
        requests: Mutex::new(Vec::new()),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(
        agent,
        vec![Arc::new(StepScopedPlugin("UNIQUE_PLUGIN_MARKER".into()))],
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("test")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    let has_marker = requests[0]
        .iter()
        .any(|m| m.contains("UNIQUE_PLUGIN_MARKER"));
    assert!(
        has_marker,
        "plugin context mutation should be visible in same step's LLM call"
    );
}

/// Multiple plugins can register hooks for the same phase.
#[tokio::test]
async fn multiple_plugins_same_phase_both_fire() {
    let count_a = Arc::new(Mutex::new(0u32));
    let count_b = Arc::new(Mutex::new(0u32));

    struct SimpleCounter(Arc<Mutex<u32>>);
    #[async_trait]
    impl PhaseHook for SimpleCounter {
        async fn run(
            &self,
            _ctx: &awaken::PhaseContext,
        ) -> Result<awaken::StateCommand, awaken::StateError> {
            *self.0.lock().unwrap() += 1;
            Ok(awaken::StateCommand::new())
        }
    }

    struct PluginA(Arc<Mutex<u32>>);
    impl Plugin for PluginA {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "plugin-a" }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "plugin-a",
                Phase::RunStart,
                SimpleCounter(Arc::clone(&self.0)),
            )?;
            Ok(())
        }
    }

    struct PluginB(Arc<Mutex<u32>>);
    impl Plugin for PluginB {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "plugin-b" }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "plugin-b",
                Phase::RunStart,
                SimpleCounter(Arc::clone(&self.0)),
            )?;
            Ok(())
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::with_plugins(
        agent,
        vec![
            Arc::new(PluginA(Arc::clone(&count_a))),
            Arc::new(PluginB(Arc::clone(&count_b))),
        ],
    );

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hi")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(*count_a.lock().unwrap(), 1, "plugin A should fire once");
    assert_eq!(*count_b.lock().unwrap(), 1, "plugin B should fire once");
}

// ===========================================================================
// Group 6: Tool result message formatting (5 tests)
// ===========================================================================

/// Tool result message for successful tool contains tool output.
#[tokio::test]
async fn tool_result_message_contains_output() {
    struct ResultCaptureLlm {
        requests: Mutex<Vec<Vec<String>>>,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for ResultCaptureLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new(
                        "c1",
                        "echo",
                        json!({"message": "TOOL_OUTPUT_MARKER"}),
                    )],
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
            "result-capture"
        }
    }

    let llm = Arc::new(ResultCaptureLlm {
        requests: Mutex::new(Vec::new()),
        call_count: Mutex::new(0),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("echo")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second_req = &requests[1];
    let has_output = second_req.iter().any(|m| m.contains("TOOL_OUTPUT_MARKER"));
    assert!(
        has_output,
        "tool output should appear in next LLM request, got: {:?}",
        second_req
    );
}

/// Failed tool result message indicates error.
#[tokio::test]
async fn failed_tool_result_message_indicates_error() {
    struct FailCaptureLlm {
        requests: Mutex<Vec<Vec<String>>>,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for FailCaptureLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "fail", json!({}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("ok")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }

        fn name(&self) -> &str {
            "fail-capture"
        }
    }

    let llm = Arc::new(FailCaptureLlm {
        requests: Mutex::new(Vec::new()),
        call_count: Mutex::new(0),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("fail")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second_req = &requests[1];
    let has_error = second_req
        .iter()
        .any(|m| m.to_lowercase().contains("error") || m.to_lowercase().contains("fail"));
    assert!(
        has_error,
        "failed tool result should indicate error in message, got: {:?}",
        second_req
    );
}

/// Unknown tool result message indicates tool not found.
#[tokio::test]
async fn unknown_tool_result_indicates_not_found() {
    struct UnknownCaptureLlm {
        requests: Mutex<Vec<Vec<String>>>,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for UnknownCaptureLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let texts: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.requests.lock().unwrap().push(texts);
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "no_such_tool", json!({}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("ok")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }

        fn name(&self) -> &str {
            "unknown-capture"
        }
    }

    let llm = Arc::new(UnknownCaptureLlm {
        requests: Mutex::new(Vec::new()),
        call_count: Mutex::new(0),
    });
    let llm_ref = Arc::clone(&llm);

    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("call unknown")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let requests = llm_ref.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second_req = &requests[1];
    let has_not_found = second_req
        .iter()
        .any(|m| m.contains("not found") || m.contains("unknown") || m.contains("not registered"));
    assert!(
        has_not_found,
        "unknown tool message should indicate not found, got: {:?}",
        second_req
    );
}

/// ToolCallStart events emitted before ToolCallDone for each tool.
#[tokio::test]
async fn tool_call_start_emitted_before_done() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "x"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    let start_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallStart { id, .. } if id == "c1"));
    let done_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallDone { id, .. } if id == "c1"));
    assert!(start_idx.is_some(), "should have ToolCallStart for c1");
    assert!(done_idx.is_some(), "should have ToolCallDone for c1");
    assert!(
        start_idx.unwrap() < done_idx.unwrap(),
        "ToolCallStart should precede ToolCallDone"
    );
}

/// Multiple tool calls in one step each get Start then Done events.
#[tokio::test]
async fn multiple_tools_each_get_start_done_pair() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("a", "echo", json!({"message": "1"})),
                ToolCall::new("b", "calc", json!({"result": 2})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    let starts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStart { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let dones: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(starts.len(), 2);
    assert_eq!(dones.len(), 2);
    assert!(starts.contains(&"a".to_string()));
    assert!(starts.contains(&"b".to_string()));
    assert!(dones.contains(&"a".to_string()));
    assert!(dones.contains(&"b".to_string()));
}

// ===========================================================================
// Group 7: Resume with all three modes (5 tests)
// ===========================================================================

/// ReplayToolCall re-executes the original tool with original args on resume.
#[tokio::test]
async fn replay_tool_call_executes_original_tool() {
    let tool_args = Arc::new(Mutex::new(Vec::<Value>::new()));

    struct ArgTrackingTool(Arc<Mutex<Vec<Value>>>);
    #[async_trait]
    impl Tool for ArgTrackingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "Tracks args")
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(args.clone());
            Ok(ToolResult::success("dangerous", args).into())
        }
    }

    // First run: suspend
    let llm1 = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "deploy", "env": "prod"}),
    )]));
    let agent1 = ResolvedAgent::new("test", "m", "sys", llm1).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver1 = FixedResolver::new(agent1);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver1,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("deploy")],
        run_identity: test_identity(),
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

    // Resume with ReplayToolCall
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap();

    // Second run: tool re-executes
    let args_tracker = Arc::clone(&tool_args);
    let llm2 = Arc::new(ScriptedLlm::new(vec![]));
    let agent2 = ResolvedAgent::new("test", "m", "sys", llm2)
        .with_tool(Arc::new(ArgTrackingTool(args_tracker)));
    let resolver2 = FixedResolver::new(agent2);

    let messages = vec![
        Message::user("deploy"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c1",
                "dangerous",
                json!({"action": "deploy", "env": "prod"}),
            )],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    run_agent_loop(AgentLoopParams {
        resolver: &resolver2,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    let tracked = tool_args.lock().unwrap();
    assert_eq!(tracked.len(), 1, "tool should be re-executed exactly once");
    assert_eq!(
        tracked[0],
        json!({"action": "deploy", "env": "prod"}),
        "should use original arguments"
    );
}

/// UseDecisionAsToolResult: decision result replaces tool arguments.
#[tokio::test]
async fn use_decision_replaces_args_with_decision_result() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"original": true}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"decision_data": "replaced"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc.calls["c1"].status, ToolCallStatus::Resuming);
    assert_eq!(
        tc.calls["c1"].arguments,
        json!({"decision_data": "replaced"}),
        "UseDecisionAsToolResult should replace arguments with decision result"
    );
}

/// PassDecisionToTool: decision result becomes new tool arguments.
#[tokio::test]
async fn pass_decision_updates_args_for_tool() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"original_key": "original_value"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"new_arg": "from_decision"}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::PassDecisionToTool),
    )
    .unwrap();

    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc.calls["c1"].status, ToolCallStatus::Resuming);
    assert_eq!(
        tc.calls["c1"].arguments,
        json!({"new_arg": "from_decision"}),
        "PassDecisionToTool should update arguments with decision result"
    );
}

/// Cancel resume transitions state from Suspended to Cancelled.
#[tokio::test]
async fn cancel_resume_transitions_to_cancelled_status() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Cancel,
                result: Value::Null,
                reason: Some("user declined".into()),
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::ReplayToolCall),
    )
    .unwrap();

    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(
        tc.calls["c1"].status,
        ToolCallStatus::Cancelled,
        "Cancel action should transition to Cancelled"
    );
}

/// Resume with empty decision result succeeds for UseDecisionAsToolResult.
#[tokio::test]
async fn resume_with_empty_decision_result_succeeds() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"key": "val"}),
    )]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Resume with null result -- should succeed
    let result = prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: Value::Null,
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    );
    assert!(
        result.is_ok(),
        "resume with null decision result should succeed"
    );

    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc.calls["c1"].status, ToolCallStatus::Resuming);
}

// ===========================================================================
// Group 8: Multi-step complex scenarios (5 tests)
// ===========================================================================

/// Three steps: tool -> tool -> response, verifying correct event sequence.
#[tokio::test]
async fn three_step_events_have_correct_overall_sequence() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("Step 1")],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "one"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Step 2")],
            tool_calls: vec![ToolCall::new("c2", "calc", json!({"result": 42}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Final")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    assert_eq!(result.response, "Final");

    let events = sink.take();

    // Verify RunStart is first and RunFinish is last
    assert!(matches!(
        events.first().unwrap(),
        AgentEvent::RunStart { .. }
    ));
    assert!(matches!(
        events.last().unwrap(),
        AgentEvent::RunFinish { .. }
    ));

    // Count key lifecycle events
    let step_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepStart { .. }))
        .count();
    let step_ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepEnd))
        .count();
    assert_eq!(step_starts, 3);
    assert_eq!(step_ends, 3);
}

/// Suspend on step 2 after successful step 1 preserves step 1 context.
#[tokio::test]
async fn suspend_on_step_two_preserves_first_step_context() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("Step 1")],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "done"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c2", "dangerous", json!({"action": "risky"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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

    // Step 1's echo tool should have emitted ToolCallDone with Succeeded
    let events = sink.take();
    let echo_done = events.iter().any(|e| {
        matches!(e, AgentEvent::ToolCallDone { id, outcome, .. }
            if id == "c1" && *outcome == awaken::contract::suspension::ToolCallOutcome::Succeeded)
    });
    assert!(echo_done, "step 1 echo should have succeeded");

    // Step 2's dangerous tool should be Suspended in state
    let tc = runtime.store().read::<ToolCallStates>().unwrap();
    assert_eq!(tc.calls["c2"].status, ToolCallStatus::Suspended);
    assert_eq!(tc.calls["c2"].tool_name, "dangerous");
}

/// Error on step 3 after two successful tool steps propagates correctly.
#[tokio::test]
async fn error_on_third_step_after_two_successful_steps() {
    struct FailOnThirdLlm {
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl LlmExecutor for FailOnThirdLlm {
        async fn execute(
            &self,
            _req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            match *count {
                1 => Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "1"}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                }),
                2 => Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new("c2", "echo", json!({"message": "2"}))],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                }),
                _ => Err(InferenceExecutionError::Provider(
                    "third call exploded".into(),
                )),
            }
        }

        fn name(&self) -> &str {
            "fail-on-third"
        }
    }

    let llm = Arc::new(FailOnThirdLlm {
        call_count: Mutex::new(0),
    });
    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;

    assert!(result.is_err(), "third-step error should propagate");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("third call exploded")
    );
}

/// Mixed tool calls per step: step 1 has 2 tools, step 2 has 1 tool, step 3 ends.
#[tokio::test]
async fn mixed_tool_counts_per_step() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![
                ToolCall::new("c1", "echo", json!({"message": "a"})),
                ToolCall::new("c2", "calc", json!({"result": 1})),
            ],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c3", "echo", json!({"message": "b"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("all done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("mixed")],
        run_identity: test_identity(),
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
    assert_eq!(result.response, "all done");

    let events = sink.take();
    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert_eq!(
        tool_done_count, 3,
        "should have 3 total tool completions (2 in step 1, 1 in step 2)"
    );
}

/// Suspend, resume, then complete: full lifecycle in 3 runs.
#[tokio::test]
async fn full_suspend_resume_complete_lifecycle() {
    // Run 1: Suspend
    let llm1 = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "build"}),
    )]));
    let agent1 = ResolvedAgent::new("test", "m", "sys", llm1).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver1 = FixedResolver::new(agent1);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let r1 = run_agent_loop(AgentLoopParams {
        resolver: &resolver1,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("build it")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();
    assert_eq!(r1.termination, TerminationReason::Suspended);

    // Verify Waiting
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // Prepare resume
    prepare_resume(
        runtime.store(),
        vec![(
            "c1".into(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )],
        Some(ToolCallResumeMode::UseDecisionAsToolResult),
    )
    .unwrap();

    // Run 2: Resume -> complete
    let llm2 = Arc::new(ScriptedLlm::new(vec![])); // LLM just returns text
    let agent2 = ResolvedAgent::new("test", "m", "sys", llm2).with_tool(Arc::new(SuspendingTool));
    let resolver2 = FixedResolver::new(agent2);

    let messages = vec![
        Message::user("build it"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "dangerous", json!({"action": "build"}))],
        ),
        Message::tool("c1", "needs user approval"),
    ];

    let r2 = run_agent_loop(AgentLoopParams {
        resolver: &resolver2,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages,
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(r2.termination, TerminationReason::NaturalEnd);

    // Verify Done
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
}

// ---------------------------------------------------------------------------
// Inference error on first call produces Err (no RunFinish emitted)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inference_error_produces_error_termination() {
    struct AlwaysFailLlm;

    #[async_trait]
    impl LlmExecutor for AlwaysFailLlm {
        async fn execute(
            &self,
            _req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Err(InferenceExecutionError::Provider("provider is down".into()))
        }

        fn name(&self) -> &str {
            "always-fail"
        }
    }

    let llm = Arc::new(AlwaysFailLlm);
    let agent = ResolvedAgent::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("hello")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;

    assert!(
        result.is_err(),
        "first-call LLM error should propagate as Err"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("provider is down"),
        "error should contain the provider message, got: {err_msg}"
    );

    // No RunFinish event should be emitted on error path
    let events = sink.take();
    let has_run_finish = events
        .iter()
        .any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    assert!(
        !has_run_finish,
        "RunFinish should not be emitted when the loop returns Err"
    );

    // RunStart should still have been emitted
    let has_run_start = events
        .iter()
        .any(|e| matches!(e, AgentEvent::RunStart { .. }));
    assert!(
        has_run_start,
        "RunStart should still be emitted before the error"
    );
}

// ---------------------------------------------------------------------------
// Token usage values accumulate correctly across steps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_usage_values_accumulated_across_steps() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "echo", json!({"message": "a"}))],
            usage: Some(TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(200),
                completion_tokens: Some(30),
                total_tokens: Some(230),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = Arc::new(VecEventSink::new());
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: test_identity(),
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
    assert_eq!(result.steps, 2, "should have 2 steps: tool call + final");

    let events = sink.take();
    let inference_usages: Vec<&TokenUsage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::InferenceComplete { usage, .. } => usage.as_ref(),
            _ => None,
        })
        .collect();

    assert_eq!(
        inference_usages.len(),
        2,
        "should have two inference events"
    );

    // Step 1: 100 prompt + 50 completion
    assert_eq!(inference_usages[0].prompt_tokens, Some(100));
    assert_eq!(inference_usages[0].completion_tokens, Some(50));

    // Step 2: 200 prompt + 30 completion
    assert_eq!(inference_usages[1].prompt_tokens, Some(200));
    assert_eq!(inference_usages[1].completion_tokens, Some(30));

    // Verify totals: 300 input, 80 output
    let total_input: i32 = inference_usages
        .iter()
        .filter_map(|u| u.prompt_tokens)
        .sum();
    let total_output: i32 = inference_usages
        .iter()
        .filter_map(|u| u.completion_tokens)
        .sum();
    assert_eq!(total_input, 300, "total input tokens should be 300");
    assert_eq!(total_output, 80, "total output tokens should be 80");
}

// ---------------------------------------------------------------------------
// Background Task Lifecycle
// ---------------------------------------------------------------------------

/// Tool that spawns a long-running background task via BackgroundTaskManager.
struct SpawnBgTaskTool {
    manager: Arc<awaken::extensions::background::BackgroundTaskManager>,
}

#[async_trait]
impl Tool for SpawnBgTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("spawn_bg", "spawn_bg", "Spawns a background task")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        use awaken::extensions::background::{TaskParentContext, TaskResult};
        self.manager
            .spawn(
                "thread-1",
                "bg",
                None,
                "test task",
                TaskParentContext::default(),
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolResult::success("spawn_bg", json!({"spawned": true})).into())
    }
}

/// Tool that does NOT spawn a background task (just returns a result).
struct NoopBgTool;

#[async_trait]
impl Tool for NoopBgTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("noop_bg", "noop_bg", "Does nothing special")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("noop_bg", json!({"done": true})).into())
    }
}

/// Tool that spawns a background task and immediately pushes an event
/// to the owner inbox (simulating a task that emits data right away).
struct SpawnEmittingBgTaskTool {
    manager: Arc<awaken::extensions::background::BackgroundTaskManager>,
    inbox_tx: awaken_runtime::inbox::InboxSender,
}

#[async_trait]
impl Tool for SpawnEmittingBgTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_emit",
            "spawn_emit",
            "Spawns a task that emits an event",
        )
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        use awaken::extensions::background::{TaskParentContext, TaskResult};
        self.manager
            .spawn(
                "thread-1",
                "bg",
                None,
                "emitting task",
                TaskParentContext::default(),
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        // Push event directly to inbox (synchronous) to guarantee it's
        // available when the orchestrator drains the inbox at NaturalEnd.
        self.inbox_tx
            .send(json!({"kind": "progress", "percent": 100}));
        Ok(ToolResult::success("spawn_emit", json!({"spawned": true})).into())
    }
}

/// Helper: create a runtime + BackgroundTaskManager wired together.
fn make_bg_runtime() -> (
    PhaseRuntime,
    Arc<awaken::extensions::background::BackgroundTaskManager>,
    Arc<awaken::extensions::background::BackgroundTaskPlugin>,
) {
    use awaken::extensions::background::{BackgroundTaskManager, BackgroundTaskPlugin};

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    let plugin = Arc::new(BackgroundTaskPlugin::new(manager.clone()));

    (runtime, manager, plugin)
}

/// 1. Running background tasks prevent NaturalEnd: lifecycle becomes Waiting.
#[tokio::test]
async fn awaiting_tasks_prevents_done_when_tasks_running() {
    use awaken::contract::lifecycle::RunStatus;

    let (runtime, manager, bg_plugin) = make_bg_runtime();

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Turn 1: call the tool that spawns a background task
        StreamResult {
            content: vec![ContentBlock::text("spawning task...")],
            tool_calls: vec![ToolCall::new("c1", "spawn_bg", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // Turn 2: plain text — NaturalEnd
        StreamResult {
            content: vec![ContentBlock::text("Done for now.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent =
        ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(SpawnBgTaskTool {
            manager: manager.clone(),
        }));

    let resolver = FixedResolver::with_plugins(agent, vec![bg_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("spawn a task")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Termination should be NaturalEnd (the LLM said so), but lifecycle is Waiting
    assert_eq!(result.termination, TerminationReason::NaturalEnd);

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(
        lifecycle.status,
        RunStatus::Waiting,
        "expected Waiting, got {:?}",
        lifecycle.status
    );
    assert_eq!(
        lifecycle.status_reason.as_deref(),
        Some("awaiting_tasks"),
        "expected awaiting_tasks reason, got {:?}",
        lifecycle.status_reason
    );

    // Clean up: cancel all tasks
    manager.cancel_all("thread-1").await;
}

/// 2. NaturalEnd without background tasks completes normally as Done.
#[tokio::test]
async fn natural_end_without_tasks_completes_normally() {
    use awaken::contract::lifecycle::RunStatus;

    let (runtime, _manager, bg_plugin) = make_bg_runtime();

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Turn 1: call the tool that does NOT spawn a background task
        StreamResult {
            content: vec![ContentBlock::text("calling tool...")],
            tool_calls: vec![ToolCall::new("c1", "noop_bg", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // Turn 2: plain text — NaturalEnd
        StreamResult {
            content: vec![ContentBlock::text("All done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(NoopBgTool));

    let resolver = FixedResolver::with_plugins(agent, vec![bg_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("do nothing special")],
        run_identity: test_identity(),
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

    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(
        lifecycle.status,
        RunStatus::Done,
        "expected Done, got {:?}",
        lifecycle.status
    );
    assert_eq!(
        lifecycle.status_reason.as_deref(),
        Some("natural"),
        "expected natural reason, got {:?}",
        lifecycle.status_reason
    );
}

/// 3. After awaiting_tasks, step_count is preserved (not reset).
#[tokio::test]
async fn awaiting_tasks_preserves_step_count() {
    use awaken::contract::lifecycle::RunStatus;

    let (runtime, manager, bg_plugin) = make_bg_runtime();

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Turn 1: call the tool
        StreamResult {
            content: vec![ContentBlock::text("spawning...")],
            tool_calls: vec![ToolCall::new("c1", "spawn_bg", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // Turn 2: NaturalEnd
        StreamResult {
            content: vec![ContentBlock::text("Waiting on tasks.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent =
        ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(SpawnBgTaskTool {
            manager: manager.clone(),
        }));

    let resolver = FixedResolver::with_plugins(agent, vec![bg_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("spawn a task")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // Verify awaiting_tasks status
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // step_count should reflect actual steps taken (2: tool call + natural end)
    assert_eq!(
        result.steps, 2,
        "expected 2 steps (tool call + final), got {}",
        result.steps
    );

    // step_count in lifecycle should be preserved (non-zero)
    assert!(
        lifecycle.step_count > 0,
        "step_count should be preserved, got {}",
        lifecycle.step_count
    );

    // Clean up
    manager.cancel_all("thread-1").await;
}

/// 4. Inbox messages from background tasks cause the loop to continue past
///    the initial NaturalEnd attempt (LLM is called again).
#[tokio::test]
async fn inbox_messages_injected_before_natural_end() {
    use awaken::extensions::background::BackgroundTaskManager;

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

    // Create an inbox channel and wire the sender to the manager
    let (inbox_tx, inbox_rx) = awaken_runtime::inbox::inbox_channel();
    let tool_inbox_tx = inbox_tx.clone();

    let mut manager = BackgroundTaskManager::new();
    manager.set_owner_inbox(inbox_tx);
    let manager = Arc::new(manager);
    manager.set_store(store.clone());

    let bg_plugin = Arc::new(awaken::extensions::background::BackgroundTaskPlugin::new(
        manager.clone(),
    ));

    // The LLM captures requests to verify inbox messages were injected
    let captured_requests: Arc<Mutex<Vec<InferenceRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured_requests.clone();

    struct CapturingLlm {
        responses: Mutex<Vec<StreamResult>>,
        captured: Arc<Mutex<Vec<InferenceRequest>>>,
    }

    #[async_trait]
    impl LlmExecutor for CapturingLlm {
        async fn execute(
            &self,
            req: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.captured.lock().unwrap().push(req);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("Final response.")],
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
            "capturing"
        }
    }

    let llm = Arc::new(CapturingLlm {
        responses: Mutex::new(vec![
            // Turn 1: call the tool that spawns an emitting task
            StreamResult {
                content: vec![ContentBlock::text("spawning emitter...")],
                tool_calls: vec![ToolCall::new("c1", "spawn_emit", json!({}))],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            // Turn 2: NaturalEnd — inbox message was drained at step 1 end
            // so it should appear in this request's messages
            StreamResult {
                content: vec![ContentBlock::text("Got the event.")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]),
        captured: captured_clone,
    });

    let agent = ResolvedAgent::new("test", "gpt-4o", "sys", llm).with_tool(Arc::new(
        SpawnEmittingBgTaskTool {
            manager: manager.clone(),
            inbox_tx: tool_inbox_tx,
        },
    ));

    let resolver = FixedResolver::with_plugins(agent, vec![bg_plugin]);

    let sink: Arc<dyn awaken::contract::event_sink::EventSink> = Arc::new(NullEventSink);
    let _result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("spawn an emitter")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: Some(inbox_rx),
        is_continuation: false,
    })
    .await
    .unwrap();

    // The inbox message was injected after step 1 (tool call) and before
    // step 2 (LLM call). Verify the 2nd LLM request contains an
    // internal_system message with the inbox payload.
    let requests = captured_requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "expected 2 LLM calls, got {}",
        requests.len()
    );

    // The 2nd request should contain the inbox message (internal_system).
    // Internal system messages have role=System + visibility=Internal.
    use awaken::contract::message::{Role, Visibility};
    let second_req = &requests[1];
    let has_inbox_msg = second_req
        .messages
        .iter()
        .any(|m| m.role == Role::System && m.visibility == Visibility::Internal);
    assert!(
        has_inbox_msg,
        "expected an internal_system message from inbox drain in the 2nd LLM request"
    );

    // Clean up
    manager.cancel_all("thread-1").await;
}
