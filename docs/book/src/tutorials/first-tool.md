# First Tool

## Goal

Implement one tool that reads and updates typed state.

> **State is optional.** Many tools (API calls, search, shell commands) don't need state — just implement `execute` and return a `ToolResult`.

## Prerequisites

- Complete [First Agent](./first-agent.md) first.
- Reuse the runtime dependencies from [First Agent](./first-agent.md).
- `State` derive is available in your dependencies:

```toml
[dependencies]
async-trait = "0.1"
serde_json = "1"
serde = { version = "1", features = ["derive"] }
tirea = "0.5.0-alpha.1"
tirea-state-derive = "0.5.0-alpha.1"
```

## 1. Define Typed State with Action

State mutations in Tirea are action-based: define an action enum and a reducer, the runtime applies changes through `ToolExecutionEffect`. Direct state writes via `ctx.state::<T>().set_*()` are rejected at runtime.

The `#[tirea(action = "...")]` attribute wires the action type and generates `StateSpec`. State scope defaults to `thread` (persists across runs); you can set `#[tirea(scope = "run")]` for per-run state or `#[tirea(scope = "tool_call")]` for per-invocation scratch data.

```rust,ignore
use serde::{Deserialize, Serialize};
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "CounterAction")]
struct Counter {
    value: i64,
    label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CounterAction {
    Increment(i64),
}

impl Counter {
    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::Increment(amount) => self.value += amount,
        }
    }
}
```

## 2. Implement the Tool

Override `execute_effect` and return state changes as typed actions via `ToolExecutionEffect`.

```rust,ignore
use async_trait::async_trait;
use serde_json::{json, Value};
use tirea::contracts::{AnyStateAction, ToolCallContext};
use tirea::contracts::runtime::tool_call::ToolExecutionEffect;
use tirea::prelude::*;

struct IncrementCounter;

#[async_trait]
impl Tool for IncrementCounter {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("increment_counter", "Increment Counter", "Increment counter state")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "amount": { "type": "integer", "default": 1 }
                }
            }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext<'_>) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx).await?.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let amount = args["amount"].as_i64().unwrap_or(1);
        let current = ctx.snapshot_of::<Counter>()
            .map(|c| c.value)
            .unwrap_or(0);

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "increment_counter",
            json!({ "before": current, "after": current + amount }),
        ))
        .with_action(AnyStateAction::new::<Counter>(
            CounterAction::Increment(amount),
        )))
    }
}
```

## 3. Register the Tool

```rust,ignore
use tirea::composition::{tool_map, AgentOsBuilder};

let os = AgentOsBuilder::new()
    .with_tools(tool_map([IncrementCounter]))
    .build()?;
```

## 4. Verify Behavior

Run one request that triggers `increment_counter`, then verify:

- Event stream contains `ToolCallDone` for `increment_counter`
- Thread state `counter.value` increases by expected amount
- Thread patch history appends at least one new patch

## 5. Reading State

Use `snapshot_of` to read the current state as a plain Rust value:

```rust,ignore
let snap = ctx.snapshot_of::<Counter>().unwrap_or_default();
println!("current value = {}", snap.value);
```

> **Note:** `ctx.state::<T>("path")` and `ctx.snapshot_at::<T>("path")` exist for advanced cases where the same state type is reused at different paths. For most tools, `snapshot_of` is the right choice — it uses the path declared on the state type automatically.

## 6. `TypedTool`

For tools with fixed argument shapes, see [`TypedTool`](../reference/typed-tool.md) — it auto-generates JSON Schema from the Rust struct and handles deserialization.

## Common Errors

- Missing derive macro import: ensure `use tirea_state_derive::State;` exists.
- Using `ctx.state::<T>().set_*()` for writes: the runtime rejects direct state writes. Use `ToolExecutionEffect` + `AnyStateAction` instead.
- Numeric parse fallback hides bugs: validate `amount` if strict input is required.
- Reaching for raw `Value` parsing too early: if your arguments map cleanly to one struct, switch to [`TypedTool`](../reference/typed-tool.md).
- Using `?` on state reads inside tool methods: `snapshot_of` returns `TireaResult<T>` but tool methods return `Result<_, ToolError>` with no `From` conversion; use `unwrap_or_default()` for types that derive `Default`.
- Forgetting `#[derive(JsonSchema)]` on `TypedTool::Args`: compilation will fail without it.

## Next

- [Build an Agent](../how-to/build-an-agent.md)
- [Add a Tool](../how-to/add-a-tool.md)
- [Typed Tool Reference](../reference/typed-tool.md)
- [State Ops Reference](../reference/state-ops.md)
