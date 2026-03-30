//! Integration tests for the runtime event lifecycle.
//!
//! Verifies the actual event sequences produced by `AgentRuntime::run()`
//! under different scenarios: simple text, max-rounds termination, and
//! tool-call flows.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::{EventSink, VecEventSink};
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};

use awaken_runtime::engine::MockLlmExecutor;
use awaken_runtime::loop_runner::build_agent_env;
use awaken_runtime::plugins::Plugin;
use awaken_runtime::registry::{AgentResolver, ResolvedAgent};
use awaken_runtime::runtime::{AgentRuntime, RunRequest};

struct ScriptedLlm {
    responses: std::sync::Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().expect("lock poisoned");
        Ok(responses.remove(0))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct SuspendOnceTool {
    calls: AtomicUsize,
}

#[async_trait]
impl Tool for SuspendOnceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("dangerous", "dangerous", "suspend once")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(ToolResult::suspended("dangerous", "needs approval"))
        } else {
            Ok(ToolResult::success("dangerous", json!({"ok": true})))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct FixedResolver {
    agent: ResolvedAgent,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken_runtime::RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&self.plugins, &agent)?;
        Ok(agent)
    }
}

fn event_type(e: &AgentEvent) -> &'static str {
    match e {
        AgentEvent::RunStart { .. } => "run_start",
        AgentEvent::RunFinish { .. } => "run_finish",
        AgentEvent::StepStart { .. } => "step_start",
        AgentEvent::StepEnd => "step_end",
        AgentEvent::TextDelta { .. } => "text_delta",
        AgentEvent::ToolCallStart { .. } => "tool_call_start",
        AgentEvent::ToolCallDelta { .. } => "tool_call_delta",
        AgentEvent::ToolCallReady { .. } => "tool_call_ready",
        AgentEvent::ToolCallDone { .. } => "tool_call_done",
        AgentEvent::InferenceComplete { .. } => "inference_complete",
        AgentEvent::StateSnapshot { .. } => "state_snapshot",
        AgentEvent::StateDelta { .. } => "state_delta",
        AgentEvent::ReasoningDelta { .. } => "reasoning_delta",
        AgentEvent::ReasoningEncryptedValue { .. } => "reasoning_encrypted_value",
        AgentEvent::MessagesSnapshot { .. } => "messages_snapshot",
        AgentEvent::ActivitySnapshot { .. } => "activity_snapshot",
        AgentEvent::ActivityDelta { .. } => "activity_delta",
        AgentEvent::ToolCallResumed { .. } => "tool_call_resumed",
        AgentEvent::ToolCallStreamDelta { .. } => "tool_call_stream_delta",
        AgentEvent::Error { .. } => "error",
    }
}

fn verify_event_ordering(events: &[AgentEvent]) {
    let types: Vec<&str> = events.iter().map(event_type).collect();

    assert!(!types.is_empty(), "no events emitted");
    assert_eq!(
        types[0], "run_start",
        "first event must be run_start, got: {types:?}"
    );
    assert_eq!(
        *types.last().unwrap(),
        "run_finish",
        "last event must be run_finish, got: {types:?}"
    );

    let mut step_depth = 0i32;
    for (i, t) in types.iter().enumerate() {
        match *t {
            "step_start" => {
                step_depth += 1;
                assert_eq!(
                    step_depth, 1,
                    "nested step_start without step_end at index {i}: {types:?}"
                );
            }
            "step_end" => {
                step_depth -= 1;
                assert!(
                    step_depth >= 0,
                    "step_end without step_start at index {i}: {types:?}"
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        step_depth, 0,
        "unclosed step: step_start without step_end: {types:?}"
    );
}

fn count_event(events: &[AgentEvent], target: &str) -> usize {
    events.iter().filter(|e| event_type(e) == target).count()
}

// ---------------------------------------------------------------------------
// Test 1: Simple text response — event sequence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn simple_text_response_event_sequence() {
    let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["Hello!".into()]));
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "You are a test assistant.", llm),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    runtime
        .run(
            RunRequest::new("thread-1", vec![Message::user("hello")]).with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await
        .expect("run should succeed");

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // Verify ordering invariants
    verify_event_ordering(&events);

    // Verify expected event types are present
    assert!(
        types.contains(&"text_delta"),
        "should contain text_delta: {types:?}"
    );
    assert!(
        types.contains(&"inference_complete"),
        "should contain inference_complete: {types:?}"
    );

    // Verify counts
    assert_eq!(count_event(&events, "run_start"), 1);
    assert_eq!(count_event(&events, "run_finish"), 1);
    assert_eq!(count_event(&events, "step_start"), 1);
    assert_eq!(count_event(&events, "step_end"), 1);

    // Verify termination reason is NaturalEnd
    if let AgentEvent::RunFinish { termination, .. } = events.last().unwrap() {
        assert_eq!(*termination, TerminationReason::NaturalEnd);
    } else {
        panic!("last event should be RunFinish");
    }
}

#[tokio::test]
async fn suspended_tool_cancel_emits_resumed_event_and_finishes() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("tools")],
            tool_calls: vec![ToolCall::new("c1", "dangerous", json!({"note": "x"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("understood, not proceeding")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));
    let tool = Arc::new(SuspendOnceTool {
        calls: AtomicUsize::new(0),
    });
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("agent", "m", "sys", llm).with_tool(tool),
        plugins: vec![],
    });
    let runtime = Arc::new(AgentRuntime::new(resolver));
    let sink = Arc::new(VecEventSink::new());

    let run_task = {
        let runtime = Arc::clone(&runtime);
        let sink = sink.clone();
        tokio::spawn(async move {
            runtime
                .run(
                    RunRequest::new("thread-deny", vec![Message::user("go")])
                        .with_agent_id("agent"),
                    sink as Arc<dyn EventSink>,
                )
                .await
        })
    };

    let mut sent = false;
    for _ in 0..40 {
        if runtime.send_decisions(
            "thread-deny",
            vec![(
                "c1".into(),
                awaken_contract::contract::suspension::ToolCallResume {
                    decision_id: "d1".into(),
                    action: awaken_contract::contract::suspension::ResumeDecisionAction::Cancel,
                    result: json!({"approved": false}),
                    reason: Some("user denied".into()),
                    updated_at: 1,
                },
            )],
        ) {
            sent = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(sent, "should send deny decision while run is active");

    let result = run_task
        .await
        .expect("join should succeed")
        .expect("run should succeed");
    assert_eq!(result.termination, TerminationReason::NaturalEnd);

    let events = sink.take();
    verify_event_ordering(&events);
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, result }
                if target_id == "c1" && result.get("approved") == Some(&json!(false))
        )),
        "deny flow should emit ToolCallResumed false: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Max rounds termination — StepEnd emitted before RunFinish
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_rounds_termination_emits_step_end() {
    let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["Response".into()]));
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "sys", llm).with_max_rounds(1),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    runtime
        .run(
            RunRequest::new("thread-max", vec![Message::user("hi")]).with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await
        .expect("run should succeed");

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // Verify ordering invariants
    verify_event_ordering(&events);

    // Verify StepEnd is emitted before RunFinish
    let step_end_idx = types.iter().rposition(|t| *t == "step_end");
    let run_finish_idx = types.iter().rposition(|t| *t == "run_finish");
    assert!(
        step_end_idx.is_some() && run_finish_idx.is_some(),
        "both step_end and run_finish should exist: {types:?}"
    );
    assert!(
        step_end_idx.unwrap() < run_finish_idx.unwrap(),
        "step_end should come before run_finish: {types:?}"
    );

    // With max_rounds=1 and a simple text response (no tool calls),
    // the run should finish naturally since the LLM returned EndTurn
    if let AgentEvent::RunFinish { termination, .. } = events.last().unwrap() {
        // NaturalEnd is expected because the mock returns EndTurn stop reason
        assert_eq!(*termination, TerminationReason::NaturalEnd);
    } else {
        panic!("last event should be RunFinish");
    }
}

// ---------------------------------------------------------------------------
// Test 3: Tool call flow — complete lifecycle
// ---------------------------------------------------------------------------

struct ToolCallMockExecutor {
    call_count: AtomicUsize,
}

#[async_trait]
impl LlmExecutor for ToolCallMockExecutor {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            // First call: return a tool call
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    "call_1",
                    "get_weather",
                    json!({"location": "Tokyo"}),
                )],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
        } else {
            // Second call: return text (after tool result)
            Ok(StreamResult {
                content: vec![ContentBlock::text("It's sunny in Tokyo")],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
    }

    fn name(&self) -> &str {
        "tool-mock"
    }
}

struct GetWeatherTool;

#[async_trait]
impl Tool for GetWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "get_weather",
            "Gets the weather for a location",
        )
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(
            "get_weather",
            json!({"temp": 25, "condition": "sunny"}),
        ))
    }
}

#[tokio::test]
async fn tool_call_flow_complete_lifecycle() {
    let llm = Arc::new(ToolCallMockExecutor {
        call_count: AtomicUsize::new(0),
    });
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(GetWeatherTool)),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    runtime
        .run(
            RunRequest::new(
                "thread-tool",
                vec![Message::user("What's the weather in Tokyo?")],
            )
            .with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await
        .expect("run should succeed");

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // Verify ordering invariants
    verify_event_ordering(&events);

    // Verify tool call events are present
    assert!(
        types.contains(&"tool_call_start"),
        "should contain tool_call_start: {types:?}"
    );
    assert!(
        types.contains(&"tool_call_done"),
        "should contain tool_call_done: {types:?}"
    );

    // Verify text delta from the second inference call
    assert!(
        types.contains(&"text_delta"),
        "should contain text_delta: {types:?}"
    );

    // Verify: 2 StepStart and 2 StepEnd (one for tool call round, one for text round)
    assert_eq!(
        count_event(&events, "step_start"),
        2,
        "expected 2 step_start events: {types:?}"
    );
    assert_eq!(
        count_event(&events, "step_end"),
        2,
        "expected 2 step_end events: {types:?}"
    );

    // Verify tool_call_done comes before the second step_start
    let tool_done_idx = types.iter().position(|t| *t == "tool_call_done").unwrap();
    let second_step_start_idx = types
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == "step_start")
        .nth(1)
        .map(|(i, _)| i)
        .unwrap();
    assert!(
        tool_done_idx < second_step_start_idx,
        "tool_call_done ({tool_done_idx}) should come before second step_start ({second_step_start_idx}): {types:?}"
    );

    // Verify termination is NaturalEnd
    if let AgentEvent::RunFinish { termination, .. } = events.last().unwrap() {
        assert_eq!(*termination, TerminationReason::NaturalEnd);
    } else {
        panic!("last event should be RunFinish");
    }
}

// ---------------------------------------------------------------------------
// Test 4: Error event on inference failure
// ---------------------------------------------------------------------------

struct FailingLlmExecutor;

#[async_trait]
impl LlmExecutor for FailingLlmExecutor {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Err(InferenceExecutionError::Provider("model overloaded".into()))
    }

    fn name(&self) -> &str {
        "failing-mock"
    }
}

#[tokio::test]
async fn error_event_emitted_on_inference_failure() {
    let llm = Arc::new(FailingLlmExecutor);
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "sys", llm),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    let result = runtime
        .run(
            RunRequest::new("thread-err", vec![Message::user("hello")]).with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await;

    // The run should return an error because inference failed
    assert!(result.is_err(), "run should fail on inference error");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("inference failed"),
        "error should be InferenceFailed, got: {err}"
    );

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // RunStart should always be the first event, even on failure
    assert!(
        !types.is_empty(),
        "at least RunStart should have been emitted"
    );
    assert_eq!(
        types[0], "run_start",
        "first event must be run_start even on failure, got: {types:?}"
    );

    // When inference fails, the error propagates before RunFinish is emitted,
    // so RunFinish should NOT be present in the stream.
    assert!(
        !types.contains(&"run_finish"),
        "run_finish should not appear when inference errors out, got: {types:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: ActivitySnapshot emitted during tool execution
// ---------------------------------------------------------------------------

struct ActivityReportingToolMockExecutor {
    call_count: AtomicUsize,
}

#[async_trait]
impl LlmExecutor for ActivityReportingToolMockExecutor {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    "call_act",
                    "reporting_tool",
                    json!({"task": "report"}),
                )],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(StreamResult {
                content: vec![ContentBlock::text("Done reporting")],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
    }

    fn name(&self) -> &str {
        "activity-mock"
    }
}

/// A tool that emits an ActivitySnapshot via the context's activity_sink.
struct ReportingTool;

#[async_trait]
impl Tool for ReportingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "reporting_tool",
            "reporting_tool",
            "Reports activity progress",
        )
    }

    async fn execute(&self, _args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        // Emit an activity snapshot through the context
        ctx.report_activity("progress", "50% complete").await;
        Ok(ToolResult::success(
            "reporting_tool",
            json!({"status": "done"}),
        ))
    }
}

#[tokio::test]
async fn activity_snapshot_emitted_during_tool_execution() {
    let llm = Arc::new(ActivityReportingToolMockExecutor {
        call_count: AtomicUsize::new(0),
    });
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(ReportingTool)),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    runtime
        .run(
            RunRequest::new("thread-activity", vec![Message::user("do task")])
                .with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await
        .expect("run should succeed");

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // Verify ordering invariants
    verify_event_ordering(&events);

    // ActivitySnapshot should be present
    assert!(
        types.contains(&"activity_snapshot"),
        "should contain activity_snapshot: {types:?}"
    );

    // ActivitySnapshot should appear between tool_call_start and tool_call_done
    let tool_start_idx = types.iter().position(|t| *t == "tool_call_start").unwrap();
    let tool_done_idx = types.iter().position(|t| *t == "tool_call_done").unwrap();
    let activity_idx = types
        .iter()
        .position(|t| *t == "activity_snapshot")
        .unwrap();

    assert!(
        activity_idx > tool_start_idx,
        "activity_snapshot ({activity_idx}) should come after tool_call_start ({tool_start_idx}): {types:?}"
    );
    assert!(
        activity_idx < tool_done_idx,
        "activity_snapshot ({activity_idx}) should come before tool_call_done ({tool_done_idx}): {types:?}"
    );

    // Verify the activity snapshot content
    let activity_event = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ActivitySnapshot { .. }))
        .unwrap();
    if let AgentEvent::ActivitySnapshot {
        activity_type,
        content,
        ..
    } = activity_event
    {
        assert_eq!(activity_type, "progress");
        assert_eq!(content, &json!("50% complete"));
    } else {
        panic!("expected ActivitySnapshot");
    }
}

// ---------------------------------------------------------------------------
// Test 6: StateSnapshot emitted after StepEnd
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_snapshot_emitted_after_step() {
    let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["Simple reply".into()]));
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("test", "m", "sys", llm),
        plugins: vec![],
    });
    let runtime = AgentRuntime::new(resolver);
    let sink = Arc::new(VecEventSink::new());

    runtime
        .run(
            RunRequest::new("thread-state", vec![Message::user("hi")]).with_agent_id("test"),
            sink.clone() as Arc<dyn EventSink>,
        )
        .await
        .expect("run should succeed");

    let events = sink.take();
    let types: Vec<&str> = events.iter().map(event_type).collect();

    // Verify ordering invariants
    verify_event_ordering(&events);

    // StateSnapshot should be present
    assert!(
        types.contains(&"state_snapshot"),
        "should contain state_snapshot: {types:?}"
    );

    // The orchestrator emits state_snapshot as part of complete_step (before step_end)
    // and also before run_finish. Verify that at least one state_snapshot exists
    // and that a state_snapshot appears before run_finish.
    let last_state_snapshot_idx = types.iter().rposition(|t| *t == "state_snapshot").unwrap();
    let run_finish_idx = types.iter().rposition(|t| *t == "run_finish").unwrap();
    assert!(
        last_state_snapshot_idx < run_finish_idx,
        "state_snapshot ({last_state_snapshot_idx}) should appear before run_finish ({run_finish_idx}): {types:?}"
    );

    // Verify that within complete_step, state_snapshot is emitted before step_end.
    // Find the first step_end and look for a state_snapshot before it.
    let first_step_end_idx = types.iter().position(|t| *t == "step_end").unwrap();
    let has_snapshot_before_step_end = types[..first_step_end_idx]
        .iter()
        .any(|t| *t == "state_snapshot");
    assert!(
        has_snapshot_before_step_end,
        "state_snapshot should appear before step_end: {types:?}"
    );

    // Verify the state snapshot is a valid JSON object
    let snapshot_event = events
        .iter()
        .find(|e| matches!(e, AgentEvent::StateSnapshot { .. }))
        .unwrap();
    if let AgentEvent::StateSnapshot { snapshot } = snapshot_event {
        assert!(
            snapshot.is_object(),
            "state snapshot should be a JSON object"
        );
    } else {
        panic!("expected StateSnapshot");
    }
}

// ---------------------------------------------------------------------------
// Test 7: Event ordering invariants on all scenarios combined
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_ordering_invariants_hold_across_scenarios() {
    // Scenario A: simple text
    {
        let llm = Arc::new(MockLlmExecutor::new());
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("test", "m", "sys", llm),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = Arc::new(VecEventSink::new());

        runtime
            .run(
                RunRequest::new("thread-inv-a", vec![Message::user("hi")]).with_agent_id("test"),
                sink.clone() as Arc<dyn EventSink>,
            )
            .await
            .expect("run should succeed");

        verify_event_ordering(&sink.take());
    }

    // Scenario B: tool call flow
    {
        let llm = Arc::new(ToolCallMockExecutor {
            call_count: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("test", "m", "sys", llm).with_tool(Arc::new(GetWeatherTool)),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = Arc::new(VecEventSink::new());

        runtime
            .run(
                RunRequest::new("thread-inv-b", vec![Message::user("weather?")])
                    .with_agent_id("test"),
                sink.clone() as Arc<dyn EventSink>,
            )
            .await
            .expect("run should succeed");

        verify_event_ordering(&sink.take());
    }

    // Scenario C: max_rounds=1 with tool call (forced early stop)
    {
        let llm = Arc::new(ToolCallMockExecutor {
            call_count: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("test", "m", "sys", llm)
                .with_max_rounds(1)
                .with_tool(Arc::new(GetWeatherTool)),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink = Arc::new(VecEventSink::new());

        runtime
            .run(
                RunRequest::new("thread-inv-c", vec![Message::user("weather?")])
                    .with_agent_id("test"),
                sink.clone() as Arc<dyn EventSink>,
            )
            .await
            .expect("run should succeed");

        let events = sink.take();
        verify_event_ordering(&events);

        // With max_rounds=1 and a tool call, the run should be stopped
        if let AgentEvent::RunFinish { termination, .. } = events.last().unwrap() {
            // After 1 round with tool use, the loop hits max_rounds
            assert!(
                matches!(termination, TerminationReason::Stopped(_)),
                "expected Stopped termination with max_rounds=1 + tool call, got: {termination:?}"
            );
        }
    }
}
