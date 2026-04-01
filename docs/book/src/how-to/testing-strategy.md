# Testing Strategy

Use this when you need to test tools, plugins, state keys, or full agent runs without depending on a live LLM.

## Prerequisites

- `awaken` crate added to `Cargo.toml` (with the runtime re-exports)
- `tokio` with `rt` and `macros` features for async tests
- `serde_json` for constructing tool arguments and assertions

## 1. Unit testing a Tool

Create a `ToolCallContext` with a test snapshot, call `tool.execute()`, and assert on the returned `ToolOutput`.

```rust,ignore
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

struct GreetTool;

#[async_trait]
impl Tool for GreetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "greet", "Greet a user by name")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'name'".into()))?;
        Ok(ToolResult::success("greet", json!({ "greeting": format!("Hello, {name}!") })).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn greet_tool_returns_greeting() {
        let tool = GreetTool;
        let ctx = ToolCallContext::test_default();

        let output = tool.execute(json!({"name": "Alice"}), &ctx).await.unwrap();

        assert!(output.result.is_success());
        assert_eq!(output.result.data["greeting"], "Hello, Alice!");
    }

    #[tokio::test]
    async fn greet_tool_rejects_missing_name() {
        let tool = GreetTool;
        let ctx = ToolCallContext::test_default();

        let err = tool.execute(json!({}), &ctx).await.unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
```

When your tool returns side-effects via `ToolOutput::with_command()`, assert on the `command` field:

```rust,ignore
#[tokio::test]
async fn tool_produces_state_command() {
    let tool = CounterMutationTool { increment: 5 };
    let ctx = ToolCallContext::test_default();

    let output = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(output.result.is_success());
    // The command is opaque at this level; integration tests (section 4)
    // verify that commands are applied correctly to the StateStore.
    assert!(!output.command.is_empty());
}
```

## 2. Unit testing a Plugin

Verify that a plugin registers the expected state keys and hooks by creating a `PluginRegistrar` and calling `plugin.register()`.

```rust,ignore
use awaken::contract::StateError;
use awaken::state::{StateKey, MergeStrategy, StateKeyOptions, StateCommand};
use awaken::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use serde::{Serialize, Deserialize};

// -- State key --

struct Counter;

impl StateKey for Counter {
    const KEY: &'static str = "test.counter";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

// -- Plugin --

struct CounterPlugin;

impl Plugin for CounterPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "counter" }
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Counter>(StateKeyOptions::default())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken::state::StateStore;

    #[test]
    fn counter_plugin_registers_key() {
        let store = StateStore::new();
        // install_plugin calls register() internally
        store.install_plugin(CounterPlugin).unwrap();

        // The key should now be readable (returns the default value)
        let val = store.read::<Counter>().unwrap_or_default();
        assert_eq!(val, 0);
    }

    #[test]
    fn counter_plugin_rejects_double_registration() {
        let store = StateStore::new();
        store.install_plugin(CounterPlugin).unwrap();

        // Registering the same key again should fail
        let result = store.install_plugin(CounterPlugin);
        assert!(result.is_err());
    }
}
```

To test a phase hook directly, build a minimal `PhaseContext` and inspect the returned `StateCommand`:

```rust,ignore
#[tokio::test]
async fn audit_hook_appends_entry() {
    let hook = AuditHook;
    let ctx = PhaseContext::test_default();

    let cmd = hook.run(&ctx).await.unwrap();

    // Commit the command to a test store and verify
    let store = StateStore::new();
    store.install_plugin(AuditPlugin).unwrap();
    store.commit(cmd).unwrap();

    let log = store.read::<AuditLogKey>().unwrap();
    assert!(!log.entries.is_empty());
}
```

## 3. Unit testing a StateKey

Test `apply()` mutations directly without any runtime overhead:

```rust,ignore
use awaken::state::{StateKey, MergeStrategy};

struct HitCounter;

impl StateKey for HitCounter {
    const KEY: &'static str = "test.hit_counter";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = u64;
    type Update = u64;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_increments_counter() {
        let mut value = u64::default();
        HitCounter::apply(&mut value, 5);
        assert_eq!(value, 5);
        HitCounter::apply(&mut value, 3);
        assert_eq!(value, 8);
    }

    #[test]
    fn apply_from_default_is_identity_free() {
        let mut value = u64::default();
        HitCounter::apply(&mut value, 0);
        assert_eq!(value, 0);
    }
}
```

For keys with complex value types, test edge cases like empty collections or merge conflicts:

```rust,ignore
#[test]
fn apply_merge_replaces_entries() {
    let mut log = AuditLog { entries: vec!["old".into()] };
    AuditLogKey::apply(&mut log, AuditLog { entries: vec!["new".into()] });
    // Exclusive merge replaces the entire value
    assert_eq!(log.entries, vec!["new"]);
}
```

## 4. Integration testing with a mock LLM

Build a full agent runtime with a scripted `LlmExecutor` that returns canned responses. This is the primary pattern used in the `awaken-runtime` integration tests.

```rust,ignore
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use serde_json::json;

use awaken::contract::content::ContentBlock;
use awaken::contract::event_sink::VecEventSink;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::loop_runner::{AgentLoopParams, LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::registry::{AgentResolver, ResolvedAgent};
use awaken::state::StateStore;
use awaken::phase::PhaseRuntime;
use awaken::RuntimeError;

// -- Scripted LLM executor --

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
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            // Fallback: end the conversation
            Ok(StreamResult {
                content: vec![ContentBlock::text("Done.")],
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

// -- Resolver --

struct FixedResolver {
    agent: ResolvedAgent,
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&[], &agent)?;
        Ok(agent)
    }
}

// -- Helpers --

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

fn test_identity() -> RunIdentity {
    RunIdentity::new(
        "thread-test".into(),
        None,
        "run-test".into(),
        None,
        "agent".into(),
        RunOrigin::User,
    )
}

// -- Test --

#[tokio::test]
async fn tool_call_flow_end_to_end() {
    // Script: LLM calls get_weather, then responds with text
    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new("c1", "get_weather", json!({"city": "Tokyo"}))]),
        text_step("The weather in Tokyo is sunny."),
    ]));

    let agent = ResolvedAgent::new("test", "model", "You are helpful.", llm)
        .with_tool(Arc::new(GetWeatherTool));

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

    let resolver = FixedResolver { agent };
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("What's the weather?")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
    })
    .await
    .unwrap();

    assert_eq!(result.response, "The weather in Tokyo is sunny.");
    assert_eq!(result.steps, 2); // tool step + text step
}
```

For simpler cases, use the built-in `MockLlmExecutor` which returns text-only responses:

```rust,ignore
use awaken::engine::MockLlmExecutor;

let llm = Arc::new(MockLlmExecutor::new().with_responses(vec!["Hello!".into()]));
let agent = ResolvedAgent::new("test", "model", "system prompt", llm);
```

## 5. Testing event streams

Use `VecEventSink` to capture all events emitted during a run and assert on their sequence and content.

```rust,ignore
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::VecEventSink;
use awaken::contract::lifecycle::TerminationReason;

#[tokio::test]
async fn events_follow_expected_lifecycle() {
    // ... set up runtime and run agent (see section 4) ...

    let events = sink.take();

    // Verify ordering: RunStart -> StepStart -> ... -> StepEnd -> RunFinish
    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(matches!(events.last(), Some(AgentEvent::RunFinish { .. })));

    // Count specific event types
    let step_starts = events.iter().filter(|e| matches!(e, AgentEvent::StepStart { .. })).count();
    let step_ends = events.iter().filter(|e| matches!(e, AgentEvent::StepEnd)).count();
    assert_eq!(step_starts, step_ends, "every StepStart needs a StepEnd");

    // Verify termination reason
    if let Some(AgentEvent::RunFinish { termination, .. }) = events.last() {
        assert_eq!(*termination, TerminationReason::NaturalEnd);
    }

    // Check that tool call events appear in the correct order
    let has_tool_start = events.iter().any(|e| matches!(e, AgentEvent::ToolCallStart { .. }));
    let has_tool_done = events.iter().any(|e| matches!(e, AgentEvent::ToolCallDone { .. }));
    if has_tool_start {
        assert!(has_tool_done, "ToolCallStart without ToolCallDone");
        let start_idx = events.iter().position(|e| matches!(e, AgentEvent::ToolCallStart { .. })).unwrap();
        let done_idx = events.iter().position(|e| matches!(e, AgentEvent::ToolCallDone { .. })).unwrap();
        assert!(start_idx < done_idx);
    }
}
```

A reusable helper for event type extraction (used in the runtime test suite):

```rust,ignore
fn event_type(e: &AgentEvent) -> &'static str {
    match e {
        AgentEvent::RunStart { .. } => "run_start",
        AgentEvent::RunFinish { .. } => "run_finish",
        AgentEvent::StepStart { .. } => "step_start",
        AgentEvent::StepEnd => "step_end",
        AgentEvent::TextDelta { .. } => "text_delta",
        AgentEvent::ToolCallStart { .. } => "tool_call_start",
        AgentEvent::ToolCallDone { .. } => "tool_call_done",
        AgentEvent::InferenceComplete { .. } => "inference_complete",
        AgentEvent::StateSnapshot { .. } => "state_snapshot",
        _ => "other",
    }
}

let types: Vec<&str> = events.iter().map(event_type).collect();
assert_eq!(types[0], "run_start");
assert_eq!(*types.last().unwrap(), "run_finish");
```

## 6. Testing with a real LLM (live tests)

For end-to-end validation against a real provider, use the `GenaiExecutor` with environment variables for credentials. Mark these tests with `#[ignore]` so they only run when explicitly requested.

```rust,ignore
use awaken::engine::GenaiExecutor;

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn live_llm_responds() {
    // Requires: OPENAI_API_KEY or (LLM_BASE_URL + LLM_API_KEY)
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let llm = Arc::new(GenaiExecutor::new());

    let agent = ResolvedAgent::new(
        "live-test",
        &model,
        "You are a test assistant. Answer in one word.",
        llm,
    );

    // ... set up resolver, store, runtime, sink as in section 4 ...

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "live-test",
        runtime: &runtime,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("What is 2+2? Answer in one word.")],
        run_identity: test_identity(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
    })
    .await
    .unwrap();

    assert!(!result.response.is_empty());
}
```

Run live tests with:

```bash
# OpenAI-compatible provider
OPENAI_API_KEY=<your-key> LLM_MODEL=gpt-4o-mini cargo test -- --ignored

# Custom endpoint (e.g. BigModel)
LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/ \
  LLM_API_KEY=<key> \
  LLM_MODEL=GLM-4.7-Flash \
  cargo test -- --ignored
```

See `examples/live_test.rs` and `examples/tool_call_live.rs` for complete working examples with console output.

## Key Files

- `crates/awaken-contract/src/contract/tool.rs` -- `Tool` trait, `ToolCallContext::test_default()`, `ToolResult`, `ToolOutput`
- `crates/awaken-contract/src/contract/event_sink.rs` -- `VecEventSink`
- `crates/awaken-runtime/src/engine/mock.rs` -- `MockLlmExecutor`
- `crates/awaken-runtime/src/state/mod.rs` -- `StateStore`, `StateCommand`
- `crates/awaken-runtime/src/loop_runner/mod.rs` -- `run_agent_loop`, `AgentLoopParams`, `AgentRunResult`
- `crates/awaken-runtime/tests/` -- integration test suite (event lifecycle, tool side effects)

## Related

- [Add a Tool](./add-a-tool.md)
- [Add a Plugin](./add-a-plugin.md)
- [Build an Agent](./build-an-agent.md)
