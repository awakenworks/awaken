#![allow(missing_docs)]
//! Comprehensive tool executor and agent loop tests ported from uncarve.
//! Covers: sequential/parallel execution, suspension, state machine transitions,
//! cross-hook state visibility, stop conditions, and phase interaction.

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::agent::executor::{ParallelToolExecutor, SequentialToolExecutor, ToolExecutor};
use awaken::agent::loop_runner::{build_agent_env, run_agent_loop};
use awaken::agent::state::{RunLifecycle, ToolCallStates};
use awaken::contract::event::AgentEvent;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::lifecycle::{RunStatus, TerminationReason};
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::profile::AgentProfile;
use awaken::contract::suspension::{ToolCallOutcome, ToolCallStatus};
use awaken::contract::tool::{Tool, ToolDescriptor, ToolError, ToolResult};
use awaken::*;
use serde::{Deserialize, Serialize};
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
                text: "Nothing more.".into(),
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
        ToolDescriptor::new("echo", "echo", "Echoes")
    }
    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("echo")
            .to_string();
        Ok(ToolResult::success_with_message("echo", args, msg))
    }
}

struct FailingTool;
#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("failing", "failing", "Fails")
    }
    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        Err(ToolError::ExecutionFailed("intentional failure".into()))
    }
}

struct SuspendingTool;
#[async_trait]
impl Tool for SuspendingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("suspending", "suspending", "Suspends")
    }
    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::suspended("suspending", "needs approval"))
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
    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ToolResult::success("counting", json!({"count": n})))
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
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions::default())?;
        Ok(())
    }
}

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let rt = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    rt
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
        text: "".into(),
        tool_calls: calls,
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
    }
}

fn text_step(text: &str) -> StreamResult {
    StreamResult {
        text: text.into(),
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    let tool_dones: Vec<_> = result
        .events
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
    assert_eq!(tool_dones[0].1, ToolCallOutcome::Succeeded);
    assert_eq!(tool_dones[1].1, ToolCallOutcome::Failed);
}

#[tokio::test]
async fn sequential_stops_after_first_suspension_in_loop() {
    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "suspending", json!({})),
        ToolCall::new("c2", "echo", json!({"message": "should not run"})),
    ])]));
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    assert_eq!(result.termination, TerminationReason::Suspended);
    let lifecycle = rt.store().read::<RunLifecycle>().unwrap();
    assert_eq!(lifecycle.status, RunStatus::Waiting);

    // Only 1 ToolCallDone (second tool should not have executed)
    let tool_dones: Vec<_> = result
        .events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .collect();
    assert_eq!(tool_dones.len(), 1);
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    let tool_dones: Vec<_> = result
        .events
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    let outcomes: Vec<_> = result
        .events
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(CountingTool {
            call_count: call_count.clone(),
        }))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    // Both tools executed (parallel doesn't stop on suspension)
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "counting tool should have executed"
    );
    let tool_dones: Vec<_> = result
        .events
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
async fn hook_state_mutation_visible_to_next_hook() {
    // Hook A writes state, Hook B reads it in the same phase
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
    let agent = AgentConfig::new("test", "m", "sys", llm);
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
    let (env, flag) = build_agent_env(&user_plugins, agent.max_rounds).unwrap();

    run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("hi")], id())
        .await
        .unwrap();

    // Reader should see the value written by Writer in the same phase
    assert_eq!(*observed.lock().unwrap(), Some(1));
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_max_rounds(3)
        .with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
async fn terminate_via_effect_in_after_inference_hook() {
    struct TerminateHook;
    #[async_trait]
    impl PhaseHook for TerminateHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.effect(RuntimeEffect::Terminate {
                reason: TerminationReason::stopped("custom_stop"),
            })
            .unwrap();
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(TermHookPlugin)];
    let (env, flag) = build_agent_env(&user_plugins, agent.max_rounds).unwrap();

    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(LogPlugin(phases.clone()))];
    let (env, flag) = build_agent_env(&user_plugins, agent.max_rounds).unwrap();

    run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(SuspendingTool));
    let rt = make_runtime();

    let user_plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(LogPlugin(phases.clone()))];
    let (env, flag) = build_agent_env(&user_plugins, agent.max_rounds).unwrap();

    run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
                .profile
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

    let llm = Arc::new(ScriptedLlm::new(vec![text_step("ok")]));
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let rt = make_runtime();

    let cfg_plugin = Arc::new(CfgPlugin(observed.clone()));
    let env = ExecutionEnv::from_plugins(&[cfg_plugin as Arc<dyn Plugin>]).unwrap();

    // Build a profile with the test section
    let profile = std::sync::Arc::new(
        AgentProfile::new("test")
            .with_section("test.model", serde_json::json!({"name": "test-model"})),
    );

    // Run phases manually with profile context
    let store = rt.store();
    let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot()).with_profile(profile);
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
        text: "Just text.".into(),
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::ToolUse), // misleading stop_reason
    }]));
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("hi")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm).with_tool(Arc::new(EchoTool));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm);
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let custom_id = RunIdentity::new(
        "t-x".into(),
        None,
        "r-x".into(),
        None,
        "a-x".into(),
        RunOrigin::Internal,
    );
    run_agent_loop(
        &agent,
        &rt,
        &env,
        &flag,
        vec![Message::user("hi")],
        custom_id,
    )
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::batch_approval()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    let tool_dones: Vec<_> = result
        .events
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(SuspendingTool))
        .with_tool(Arc::new(CountingTool {
            call_count: call_count.clone(),
        }))
        .with_tool_executor(Arc::new(ParallelToolExecutor::batch_approval()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
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
    let agent = AgentConfig::new("test", "m", "sys", llm)
        .with_tool(Arc::new(EchoTool))
        .with_tool(Arc::new(FailingTool))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let rt = make_runtime();
    let (env, flag) = build_agent_env(&[], agent.max_rounds).unwrap();
    let result = run_agent_loop(&agent, &rt, &env, &flag, vec![Message::user("go")], id())
        .await
        .unwrap();

    let outcomes: Vec<_> = result
        .events
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
