#![allow(missing_docs)]
//! Integration tests verifying ToolOutput.command side-effects.
//!
//! Tools can now produce state mutations, scheduled actions, and effects via
//! `ToolOutput::with_command()`, using the same `StateCommand` as plugin hooks.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::StateError;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::event_sink::VecEventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::inference::{StopReason, StreamResult};
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::state::{MergeStrategy, StateKey, StateKeyOptions};

use awaken_runtime::RuntimeError;
use awaken_runtime::agent::state::{
    AddContextMessage, ContextMessageStore, ContextThrottleState, RunLifecycle, ToolCallStates,
};
use awaken_runtime::execution::ParallelToolExecutor;
use awaken_runtime::loop_runner::{AgentLoopParams, build_agent_env, run_agent_loop};
use awaken_runtime::phase::PhaseRuntime;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::registry::{AgentResolver, ResolvedAgent};
use awaken_runtime::state::{StateCommand, StateStore};

// ---------------------------------------------------------------------------
// State keys
// ---------------------------------------------------------------------------

struct Counter;

impl StateKey for Counter {
    const KEY: &'static str = "test.tool_side_effects.counter";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

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

struct CounterPlugin;

impl Plugin for CounterPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "counter-plugin",
        }
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Counter>(StateKeyOptions::default())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
    captured_requests: Mutex<Vec<InferenceRequest>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
            captured_requests: Mutex::new(Vec::new()),
        }
    }

    fn captured_requests(&self) -> Vec<InferenceRequest> {
        self.captured_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.captured_requests.lock().unwrap().push(req);
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

/// Tool that mutates Counter state via its command.
struct CounterMutationTool {
    increment: usize,
}

#[async_trait]
impl Tool for CounterMutationTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("counter_mutation", "counter_mutation", "Increments counter")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let mut cmd = StateCommand::new();
        cmd.update::<Counter>(self.increment);
        Ok(ToolOutput::with_command(
            ToolResult::success(
                "counter_mutation",
                json!({"incremented_by": self.increment}),
            ),
            cmd,
        ))
    }
}

/// Tool that schedules an AddContextMessage action via its command.
struct ContextInjectorTool;

#[async_trait]
impl Tool for ContextInjectorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("context_injector", "context_injector", "Injects context")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let msg = ContextMessage::system_persistent(
            "tool.injected_context",
            "You have been augmented by the context_injector tool.",
        );
        let mut cmd = StateCommand::new();
        cmd.schedule_action::<AddContextMessage>(msg)
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        Ok(ToolOutput::with_command(
            ToolResult::success("context_injector", json!({"injected": true})),
            cmd,
        ))
    }
}

/// Simple tool that returns ToolResult::success(...).into() with no command.
struct PlainTool;

#[async_trait]
impl Tool for PlainTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("plain", "plain", "No side-effects")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("plain", json!({"ok": true})).into())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let rt = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    rt
}

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
// TESTS
// ===========================================================================

/// Test 1: Tool state mutation applied after execution.
///
/// A custom tool returns `ToolOutput::with_command()` containing a Counter state
/// mutation. The Counter key is registered via a plugin. After the agent loop
/// completes, the state store should reflect the mutation.
#[tokio::test]
async fn tool_state_mutation_applied_after_execution() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new("c1", "counter_mutation", json!({}))]),
        text_step("Done."),
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone())
        .with_tool(Arc::new(CounterMutationTool { increment: 5 }));

    let rt = make_runtime();
    rt.store().install_plugin(CounterPlugin).unwrap();

    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(CounterPlugin)]);
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

    // Verify the Counter state was updated by the tool's command
    let counter_val = rt.store().read::<Counter>().expect("Counter should exist");
    assert_eq!(counter_val, 5, "Counter should have been incremented by 5");
}

/// Test 2: Tool scheduled action executed.
///
/// A tool schedules an AddContextMessage action via its command. After the tool
/// call, the context message should be stored in ContextMessageStore and visible
/// to subsequent inference calls.
#[tokio::test]
async fn tool_scheduled_action_executed() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // Step 1: LLM calls the context_injector tool
        tool_step(vec![ToolCall::new("c1", "context_injector", json!({}))]),
        // Step 2: LLM produces a second tool call (so we get a second inference request
        // where we can verify the context message was injected)
        tool_step(vec![ToolCall::new("c2", "plain", json!({}))]),
        // Step 3: LLM ends
        text_step("Done."),
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone())
        .with_tool(Arc::new(ContextInjectorTool))
        .with_tool(Arc::new(PlainTool));

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

    // Verify the context message was stored
    let store_value = rt
        .store()
        .read::<ContextMessageStore>()
        .expect("ContextMessageStore should exist after action");
    assert!(
        store_value.messages.contains_key("tool.injected_context"),
        "Context message with key 'tool.injected_context' should be in the store"
    );
    let msg = &store_value.messages["tool.injected_context"];
    assert!(msg.content.iter().any(|c| match c {
        ContentBlock::Text { text, .. } => text.contains("augmented by the context_injector"),
        _ => false,
    }));

    // Verify the context message appeared in a subsequent inference request.
    // The second inference (index 1) should contain the injected context message
    // because it runs BeforeInference phase which processes the scheduled action.
    let requests = llm.captured_requests();
    assert!(
        requests.len() >= 2,
        "Expected at least 2 inference requests, got {}",
        requests.len()
    );

    let second_request = &requests[1];
    let all_text: String = second_request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        all_text.contains("augmented by the context_injector"),
        "Second inference request should contain the injected context message. Messages: {:?}",
        second_request
            .messages
            .iter()
            .map(|m| format!("{:?}: {:?}", m.role, m.content))
            .collect::<Vec<_>>()
    );
}

/// Test 3: Tool with empty command has no side effects.
///
/// A tool returns `ToolResult::success(...).into()` (which creates a ToolOutput
/// with an empty StateCommand). No extra state changes should occur beyond the
/// standard lifecycle updates.
#[tokio::test]
async fn tool_empty_command_has_no_side_effects() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new("c1", "plain", json!({}))]),
        text_step("Done."),
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone()).with_tool(Arc::new(PlainTool));

    let rt = make_runtime();
    // Register Counter plugin but the tool should NOT mutate it
    rt.store().install_plugin(CounterPlugin).unwrap();

    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(CounterPlugin)]);
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

    // Counter should not exist (never set) or be default (0)
    let counter_val = rt.store().read::<Counter>().unwrap_or_default();
    assert_eq!(
        counter_val, 0,
        "Counter should remain at default 0 when tool produces no command"
    );

    // ContextMessageStore should be empty (no scheduled actions)
    let ctx_store = rt.store().read::<ContextMessageStore>().unwrap_or_default();
    assert!(
        ctx_store.messages.is_empty(),
        "No context messages should be stored when tool produces empty command"
    );
}

/// Test 4: Parallel tool commands merge.
///
/// Two tools that both return state mutations to a Commutative key (Counter)
/// are executed via ParallelToolExecutor. Both mutations should be applied.
#[tokio::test]
async fn parallel_tool_commands_merge() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // LLM calls both counter tools in one step
        tool_step(vec![
            ToolCall::new("c1", "counter_mutation", json!({})),
            ToolCall::new("c2", "counter_mutation", json!({})),
        ]),
        text_step("Done."),
    ]));

    let agent = ResolvedAgent::new("test", "m", "sys", llm.clone())
        .with_tool(Arc::new(CounterMutationTool { increment: 3 }))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    rt.store().install_plugin(CounterPlugin).unwrap();

    let resolver = FixedResolver::with_plugins(agent, vec![Arc::new(CounterPlugin)]);
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

    // Both tools increment by 3, so the total should be 6
    let counter_val = rt.store().read::<Counter>().expect("Counter should exist");
    assert_eq!(
        counter_val, 6,
        "Both parallel tool mutations should be applied (3 + 3 = 6)"
    );
}
