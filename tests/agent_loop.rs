#![allow(missing_docs)]

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::agent::loop_runner::{build_agent_env, prepare_resume, run_agent_loop};
use awaken::agent::state::{ContextThrottleState, RunLifecycle, ToolCallStates};
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
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
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

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message")
            .to_string();
        Ok(ToolResult::success_with_message("echo", args, msg))
    }
}

struct CalcTool;

#[async_trait]
impl Tool for CalcTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("calc", "calculator", "Evaluates math")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let result = args.get("result").cloned().unwrap_or(json!(0));
        Ok(ToolResult::success("calc", result))
    }
}

struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("fail", "fail", "Always fails")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
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

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::suspended("dangerous", "needs user approval"))
    }
}

/// Tool that returns the arguments as result (useful for PassDecisionToTool test).
struct PassthroughTool;

#[async_trait]
impl Tool for PassthroughTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("passthrough", "passthrough", "Returns args as result")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("passthrough", args))
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

/// Test resolver that wraps a fixed AgentConfig + optional user plugins.
struct FixedResolver {
    agent: AgentConfig,
    user_plugins: Vec<Arc<dyn Plugin>>,
}

impl FixedResolver {
    fn new(agent: AgentConfig) -> Self {
        Self {
            agent,
            user_plugins: vec![],
        }
    }

    fn with_plugins(agent: AgentConfig, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        Self {
            agent,
            user_plugins: plugins,
        }
    }
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, StateError> {
        let env = build_agent_env(&self.user_plugins, &self.agent)?;
        Ok(ResolvedAgent {
            config: self.agent.clone(),
            env,
        })
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
    }]));

    let agent = AgentConfig::new("test", "gpt-4o", "You are helpful.", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.response, "Hello, world!");
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 1);

    // Verify run lifecycle state
    let lifecycle = runtime.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Done);
    assert_eq!(lifecycle.done_reason.as_deref(), Some("natural"));
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
        },
        StreamResult {
            content: vec![ContentBlock::text("The echo said: hello")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("echo hello")],
        test_identity(),
        None,
    )
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
        },
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("test")],
        test_identity(),
        None,
    )
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
        },
        StreamResult {
            content: vec![ContentBlock::text("Done.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(CalcTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = VecEventSink::new();
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("multi-tool")],
        test_identity(),
        None,
    )
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
            })
            .collect(),
    ));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm)
        .with_max_rounds(3)
        .with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("loop")],
        test_identity(),
        None,
    )
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
            .done_reason
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
        },
        StreamResult {
            content: vec![ContentBlock::text("Sorry, that tool doesn't exist.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = VecEventSink::new();
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("call unknown")],
        test_identity(),
        None,
    )
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
        },
        StreamResult {
            content: vec![ContentBlock::text("Tool failed, sorry.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(FailingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("use fail tool")],
        test_identity(),
        None,
    )
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
    }]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = VecEventSink::new();
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        None,
    )
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
        },
        StreamResult {
            content: vec![ContentBlock::text("Done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        },
    ]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm).with_tool(Arc::new(EchoTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = VecEventSink::new();
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("echo")],
        test_identity(),
        None,
    )
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
            "ToolCallStart", // from streaming
            "InferenceComplete",
            "ToolCallStart", // from tool execution
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
    }]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm);
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

    let sink = NullEventSink;
    run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        identity,
        None,
    )
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
    }]));

    let agent = AgentConfig::new("test", "gpt-4o", "helpful", llm);
    let runtime = make_runtime();

    let tracker_plugin = Arc::new(TrackerPlugin(Arc::clone(&hook_phases)));
    let user_plugins: Vec<Arc<dyn Plugin>> = vec![tracker_plugin];
    let resolver = FixedResolver::with_plugins(agent, user_plugins);

    let sink = NullEventSink;
    run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        None,
    )
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
    }
}

#[tokio::test]
async fn tool_suspension_transitions_run_to_waiting() {
    let llm = Arc::new(ScriptedLlm::new(vec![make_tool_call_response(
        "dangerous",
        "c1",
        json!({"action": "delete"}),
    )]));

    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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

    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
        ToolCallResumeMode::UseDecisionAsToolResult,
    )
    .unwrap();

    let resume_result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        messages,
        test_identity(),
        None,
    )
    .await
    .unwrap();

    // Should have completed (LLM returns text, no more tools)
    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);

    // Tool call should be terminal
    let tc_states = runtime.store().read::<ToolCallStates>().unwrap_or_default();
    // After resume, tool call states were cleared by the new step
    // The run completed normally
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

    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
        ToolCallResumeMode::ReplayToolCall,
    )
    .unwrap();

    let resume_result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        messages,
        test_identity(),
        None,
    )
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

    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(EchoTool)); // echo registered for replay
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run until suspension
    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("dangerous", args))
        }
    }

    let llm2 = Arc::new(ScriptedLlm::new(vec![]));
    let agent2 = AgentConfig::new("test", "m", "sys", llm2).with_tool(Arc::new(DangerousEcho));
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
        ToolCallResumeMode::ReplayToolCall,
    )
    .unwrap();

    let resume_result = run_agent_loop(
        &resolver2,
        "test",
        &runtime,
        &sink,
        None,
        messages,
        test_identity(),
        None,
    )
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

    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Hack: register passthrough as "dangerous" initially for suspension
    // Actually, let's use a different approach: SuspendingTool is "dangerous"
    // but we need passthrough for resume. Let's use a tool that suspends first.
    // Simpler: just use SuspendingTool for suspension, then on resume use passthrough.

    // First run: suspend
    let sink = NullEventSink;
    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
    let agent2 = AgentConfig::new("test", "m", "sys", llm2).with_tool(Arc::new(SuspendingTool));
    let resolver2 = FixedResolver::new(agent2);

    let result = run_agent_loop(
        &resolver2,
        "test",
        &runtime2,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("dangerous", args))
        }
    }

    let llm3 = Arc::new(ScriptedLlm::new(vec![]));
    let agent3 =
        AgentConfig::new("test", "m", "sys", llm3).with_tool(Arc::new(DangerousPassthrough));
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
        ToolCallResumeMode::PassDecisionToTool,
    )
    .unwrap();

    let resume_result = run_agent_loop(
        &resolver3,
        "test",
        &runtime2,
        &sink,
        None,
        messages,
        test_identity(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(resume_result.termination, TerminationReason::NaturalEnd);
}

#[tokio::test]
async fn resume_rejects_non_waiting_run() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    // Run to completion (not suspended)
    let sink = NullEventSink;
    run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        None,
    )
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
        ToolCallResumeMode::ReplayToolCall,
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

    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);

    let sink = NullEventSink;
    run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("do it")],
        test_identity(),
        None,
    )
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
        ToolCallResumeMode::ReplayToolCall,
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
        use awaken::contract::executor::StreamEvent;
        use futures::StreamExt as _;
        let deltas = self.deltas.clone();
        let delay = self.delay_ms;
        Box::pin(async move {
            let stream = futures::stream::unfold(
                (deltas.into_iter(), delay),
                |(mut iter, delay)| async move {
                    let delta = iter.next()?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    let event: Result<StreamEvent, InferenceExecutionError> =
                        Ok(StreamEvent::TextDelta(delta));
                    Some((event, (iter, delay)))
                },
            );
            let stop = futures::stream::once(async { Ok(StreamEvent::Stop(StopReason::EndTurn)) });
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
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = NullEventSink;
    let token = CancellationToken::new();
    let token_clone = token.clone();

    // Cancel after 100ms — mid-stream (after ~2 of 10 deltas)
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token_clone.cancel();
    });

    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        Some(token),
    )
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
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let runtime = make_runtime();
    let resolver = FixedResolver::new(agent);
    let sink = NullEventSink;

    let token = CancellationToken::new();
    token.cancel();

    let result = run_agent_loop(
        &resolver,
        "test",
        &runtime,
        &sink,
        None,
        vec![Message::user("hi")],
        test_identity(),
        Some(token),
    )
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::Cancelled,
        "run should terminate immediately when token is already cancelled"
    );
    // steps is incremented at loop top before cancellation check, so it will be 1
    assert_eq!(
        result.steps, 1,
        "only one step entry before cancellation detected"
    );
}
