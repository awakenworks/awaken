# First Tool

## Goal

Implement one tool that reads typed state from `ToolCallContext` during execution.

> **State is optional.** Many tools (API calls, search, shell commands) don't need state -- just implement `execute` and return a `ToolResult`.

## Prerequisites

- Complete [First Agent](./first-agent.md) first.
- Reuse the runtime dependencies from [First Agent](./first-agent.md).

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

## 1. Define a `StateKey`

A `StateKey` describes one named slot in the state map. It declares the value type, how updates are applied, and the lifetime scope.

```rust,no_run
# use awaken::{StateKey, KeyScope, MergeStrategy};
#
/// Tracks how many times the greeting tool has been called.
struct GreetCount;

impl StateKey for GreetCount {
    const KEY: &'static str = "greet_count";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = u32;
    type Update = u32;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}
# fn main() {}
```

Key choices:

- `KeyScope::Run` -- state resets at the start of each run. Use `KeyScope::Thread` to persist across runs.
- `MergeStrategy::Commutative` -- safe for concurrent updates. Use `Exclusive` when only one writer is expected.
- `apply` defines how an `Update` modifies the current `Value`. Here it increments.

## 2. Implement the Tool

The tool reads the current count via `ctx.state::<GreetCount>()` and returns a personalized greeting.

```rust,no_run
# use awaken::{StateKey, KeyScope, MergeStrategy};
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{json, Value};
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};

struct GreetTool;

#[async_trait]
impl Tool for GreetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "Greet", "Greet a user by name")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name to greet" }
                },
                "required": ["name"]
            }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        args["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("name is required".into()))?;
        Ok(())
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let name = args["name"].as_str().unwrap_or("world");

        // Read state -- returns None if the key has not been set yet.
        let count = ctx.state::<GreetCount>().copied().unwrap_or(0);

        Ok(ToolResult::success("greet", json!({
            "greeting": format!("Hello, {}!", name),
            "times_greeted": count,
        })).into())
    }
}
# fn main() {}
```

## 3. Register the Tool

```rust,no_run
# use std::sync::Arc;
# use async_trait::async_trait;
# use serde_json::{json, Value};
# use awaken::{StateKey, KeyScope, MergeStrategy};
# use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
# struct GreetTool;
# #[async_trait]
# impl Tool for GreetTool {
#     fn descriptor(&self) -> ToolDescriptor {
#         ToolDescriptor::new("greet", "Greet", "Greet a user by name")
#     }
#     async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
#         Ok(ToolResult::success("greet", json!({})).into())
#     }
# }
use awaken::registry_spec::AgentSpec;
use awaken::AgentRuntimeBuilder;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let agent_spec = AgentSpec::new("assistant")
    .with_model("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant. Use the greet tool when asked.")
    .with_max_rounds(5);

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("greet", Arc::new(GreetTool))
    .build()?;
# Ok(())
# }
```

## 4. Run

```rust,no_run
# use std::sync::Arc;
# use async_trait::async_trait;
# use serde_json::{json, Value};
# use awaken::{StateKey, KeyScope, MergeStrategy};
# use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
# use awaken::registry_spec::AgentSpec;
# use awaken::AgentRuntimeBuilder;
# struct GreetCount;
# impl StateKey for GreetCount {
#     const KEY: &'static str = "greet_count";
#     const MERGE: MergeStrategy = MergeStrategy::Commutative;
#     const SCOPE: KeyScope = KeyScope::Run;
#     type Value = u32;
#     type Update = u32;
#     fn apply(value: &mut Self::Value, update: Self::Update) { *value += update; }
# }
# struct GreetTool;
# #[async_trait]
# impl Tool for GreetTool {
#     fn descriptor(&self) -> ToolDescriptor {
#         ToolDescriptor::new("greet", "Greet", "Greet a user by name")
#     }
#     async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
#         Ok(ToolResult::success("greet", json!({})).into())
#     }
# }
use awaken::contract::message::Message;
use awaken::contract::event_sink::VecEventSink;
use awaken::RunRequest;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let runtime = AgentRuntimeBuilder::new()
#     .with_agent_spec(AgentSpec::new("assistant").with_model("gpt-4o-mini"))
#     .with_tool("greet", Arc::new(GreetTool))
#     .build()?;
let request = RunRequest::new(
    "thread-1",
    vec![Message::user("Greet Alice")],
)
.with_agent_id("assistant");

let sink = Arc::new(VecEventSink::new());
runtime.run(request, sink.clone()).await?;
# Ok(())
# }
```

## 5. Verify

Check the collected events for a `ToolCallDone` event with `name == "greet"`:

```rust,ignore
use awaken::contract::event::AgentEvent;

let events = sink.take();
let tool_done = events.iter().any(|e| matches!(
    e,
    AgentEvent::ToolCallDone { id: _, message_id: _, result: _, outcome: _ }
));
println!("tool_call_done_seen: {}", tool_done);
```

Expected:

- `tool_call_done_seen: true`
- The `result` inside `ToolCallDone` contains `greeting` and `times_greeted` fields.

## What You Created

A tool that:

1. Declares a JSON Schema for its arguments via `descriptor()`.
2. Validates arguments before execution via `validate_args()`.
3. Reads typed state from the snapshot via `ctx.state::<K>()`.
4. Returns structured JSON via `ToolResult::success()`.

The `StateKey` trait gives you type-safe, scoped state without raw JSON manipulation.

## Which Doc To Read Next

- understand the full tool lifecycle: [Tool Trait](../reference/tool-trait.md)
- add plugins that manage state across runs: [Add a Plugin](../how-to/add-a-plugin.md)
- learn about state scoping rules: [State Keys](../reference/state-keys.md)

## Common Errors

- `ctx.state::<K>()` returns `None`: the state key has not been written yet in this run. Use `.unwrap_or_default()` or `.copied().unwrap_or(0)` for numeric defaults.
- `StateError::KeyEncode` / `StateError::KeyDecode`: the `Value` type does not round-trip through JSON. Ensure `Serialize` and `Deserialize` are derived correctly.
- `ToolError::InvalidArguments` not surfaced: `validate_args` is called before `execute` by the runtime. If you skip validation, bad input reaches `execute` and may panic on `.unwrap()`.
- Scope mismatch: `KeyScope::Run` state is cleared between runs. If you expect persistence, use `KeyScope::Thread`.
