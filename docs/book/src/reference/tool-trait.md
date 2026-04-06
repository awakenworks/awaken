# Tool Trait

The `Tool` trait is the primary extension point for giving agents capabilities.
Tools are stateless functions that receive JSON arguments and a read-only context,
and return a `ToolOutput`.

## Trait definition

```rust,ignore
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the descriptor for this tool.
    fn descriptor(&self) -> ToolDescriptor;

    /// Validate arguments before execution. Default: accept all.
    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    /// Execute the tool with the given arguments and context.
    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError>;
}
```

**Crate path:** `awaken::contract::tool::Tool`

## ToolDescriptor

Describes a tool's identity and parameter schema. Registered with the runtime
and sent to the LLM as available functions.

```rust,ignore
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
    pub category: Option<String>,
}
```

Builder methods:

```rust,ignore
ToolDescriptor::new(id, name, description) -> Self
    .with_parameters(schema: Value) -> Self
    .with_category(category: impl Into<String>) -> Self
```

## ToolResult

Returned by `Tool::execute`. Carries the execution outcome back to the agent loop.

```rust,ignore
pub struct ToolResult {
    pub tool_name: String,
    pub status: ToolStatus,
    pub data: Value,
    pub message: Option<String>,
    pub suspension: Option<Box<SuspendTicket>>,
}
```

### ToolStatus

```rust,ignore
pub enum ToolStatus {
    Success,  // Execution succeeded
    Pending,  // Suspended, waiting for external resume
    Error,    // Execution failed; content sent back to LLM
}
```

### Constructors

| Method | Status | Use case |
|---|---|---|
| `ToolResult::success(name, data)` | `Success` | Normal completion |
| `ToolResult::success_with_message(name, data, msg)` | `Success` | Completion with description |
| `ToolResult::error(name, message)` | `Error` | Recoverable failure |
| `ToolResult::error_with_code(name, code, message)` | `Error` | Structured error with code |
| `ToolResult::suspended(name, message)` | `Pending` | HITL suspension |
| `ToolResult::suspended_with(name, message, ticket)` | `Pending` | Suspension with ticket |

### Predicates

- `is_success() -> bool`
- `is_pending() -> bool`
- `is_error() -> bool`
- `to_json() -> Value`

## ToolError

Errors returned from `validate_args` or `execute`. Unlike `ToolResult::Error`
(which is sent to the LLM), a `ToolError` aborts the tool call.

```rust,ignore
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

## ToolCallContext

Read-only context provided to a tool during execution.

```rust,ignore
pub struct ToolCallContext {
    pub call_id: String,
    pub tool_name: String,
    pub run_identity: RunIdentity,
    pub agent_spec: Arc<AgentSpec>,
    pub snapshot: Snapshot,
    pub activity_sink: Option<Arc<dyn EventSink>>,
    pub cancellation_token: Option<CancellationToken>,
    pub resume_input: Option<ToolCallResume>,
    pub suspension_id: Option<String>,
    pub suspension_reason: Option<String>,
}
```

- `resume_input` is populated when this execution is a replay of a suspended
  tool call.
- `suspension_id` / `suspension_reason` identify the active suspension being
  replayed.
- Frontend tools and suspension-aware backend tools should inspect these fields
  instead of inferring resume state from arguments alone.

### Methods

```rust,ignore
/// Read a typed state key from the snapshot.
fn state<K: StateKey>(&self) -> Option<&K::Value>

/// Report an activity snapshot for this tool call.
async fn report_activity(&self, activity_type: &str, content: &str)

/// Report an incremental activity delta.
async fn report_activity_delta(&self, activity_type: &str, patch: Value)

/// Report structured tool call progress.
async fn report_progress(
    &self,
    status: ProgressStatus,
    message: Option<&str>,
    progress: Option<f64>,
)
```

## Examples

### Minimal tool

```rust,ignore
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};
use serde_json::{Value, json};

struct Greet;

#[async_trait]
impl Tool for Greet {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "greet", "Greet a user by name")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("name required".into()))?;
        Ok(ToolResult::success("greet", json!({ "greeting": format!("Hello, {name}!") })).into())
    }
}
```

### Reading state from context

```rust,ignore
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};
use awaken::state::StateKey;
use serde::{Serialize, Deserialize};
use serde_json::{Value, json};

// Assume a state key is defined elsewhere:
// struct UserPreferences;
// impl StateKey for UserPreferences { ... }

struct GetPreferences;

#[async_trait]
impl Tool for GetPreferences {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_prefs", "get_preferences", "Get user preferences")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        // Read typed state through the snapshot
        // let prefs = ctx.state::<UserPreferences>().cloned().unwrap_or_default();
        // Ok(ToolResult::success("get_prefs", serde_json::to_value(&prefs).unwrap()).into())
        Ok(ToolResult::success("get_prefs", json!({})).into())
    }
}
```

## Tool execution hooks

Every tool call passes through plugin hooks before and after execution. This
allows plugins to intercept, observe, or modify tool call behavior without
changing the tool itself.

### Full lifecycle

```text
LLM selects tool
  -> validate_args()
  -> BeforeToolExecute phase (plugins run hooks)
     Plugins may schedule ToolInterceptAction:
       Block    -> run terminates with reason
       Suspend  -> run pauses (HITL), tool not executed
       SetResult -> tool skipped, pre-built result used
  -> execute()                (only if not intercepted)
  -> AfterToolExecute phase   (plugins run hooks)
     Plugins observe the ToolResult and may modify state
```

### BeforeToolExecute

Runs once per tool call, after argument validation. Plugins receive a
`PhaseContext` containing the tool name, call ID, and validated arguments.
Hooks return a `StateCommand` that may schedule `ToolInterceptAction` to
block, suspend, or short-circuit the call.

When multiple intercepts are scheduled, priority is:
**Block > Suspend > SetResult**.

### AfterToolExecute

Runs after `execute()` completes (or after an intercept produces a result).
Plugins receive the `PhaseContext` with the `ToolResult` attached. Hooks can
update state, emit events, or schedule actions for subsequent phases.

### ToolCallStatus transitions

Each tool call tracks a `ToolCallStatus` through its lifecycle:

```text
New -> Running -> Succeeded
                  Failed
                  Suspended -> Resuming -> Running -> ...
                  Cancelled
```

Terminal states (`Succeeded`, `Failed`, `Cancelled`) cannot transition further.

See [Plugin Internals](../explanation/plugin-internals.md) for intercept
priority details and the full phase convergence loop.

## Related

- [First Tool Tutorial](../tutorials/first-tool.md)
- [Add a Tool](../how-to/add-a-tool.md)
